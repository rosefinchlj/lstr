//! 候选生成:精确 / 前缀 / 子串(打包 buffer memmem)+ 拼音缓存。
//!
//! 语义对齐现有链路(方案 §4.3):
//! - 候选 = 名字匹配(多 token AND);
//! - 收集封顶 candidate_cap,total 照常精确统计(全扫计数);
//! - 精确/前缀不单独走二分:memmem 一次扫出全部命中,按「命中起点/长度」
//!   判定精确与前缀(ready 模式的二分加速留作 P4 杠杆);
//! - scan 模式(未 compact,无 offs)同样适用,渐进搜索白拿;
//! - 名字区 `\0` 结尾保证命中不跨条目,仍按起点校验兜底。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use regex::bytes::{Regex, RegexBuilder};

use crate::store::{fold_case, VolumeStore, ATTR_INTERNAL, NO_PARENT};

pub const FILE_ATTRIBUTE_DIRECTORY_U16: u16 = 0x0010;

/// 文件名严格匹配谓词（区分大小写 / 全词）。
///
/// 现有 ngram / char_mask / memmem 仍作为大小写不敏感的子串超集预筛；
/// 这里在 emit 前再做一次严格校验，保证 cap/total 只统计真正命中的条目。
pub(crate) struct MatchMatcher {
    tokens: Vec<Vec<u8>>,
    whole_word: bool,
    case_sensitive: bool,
}

impl MatchMatcher {
    pub(crate) fn new(spec: &QuerySpec) -> Option<Self> {
        if spec.regex.is_some() || (!spec.case_sensitive && !spec.whole_word) {
            return None;
        }
        let tokens: Vec<Vec<u8>> = spec
            .tokens
            .iter()
            .filter(|token| !token.is_empty())
            .map(|token| {
                if spec.case_sensitive {
                    token.as_bytes().to_vec()
                } else {
                    fold_case(token.as_bytes())
                }
            })
            .collect();
        Some(Self {
            tokens,
            whole_word: spec.whole_word,
            case_sensitive: spec.case_sensitive,
        })
    }

    pub(crate) fn matches(&self, store: &VolumeStore, slot: u32) -> bool {
        let name = if self.case_sensitive {
            store.name(slot)
        } else {
            store.name_lc(slot)
        };
        self.tokens
            .iter()
            .all(|token| token_matches(name, token, self.whole_word))
    }
}

/// ASCII 字母数字算“词字符”；其余（标点/空白/非 ASCII 字节）都算边界。
#[inline]
fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
}

// ---------- 正则模式 ----------

/// 单条缓存（同一 worker 顺序查一个正则；换模式自然替换）。
fn regex_cache() -> &'static Mutex<Option<(String, bool, Arc<Regex>)>> {
    static CACHE: OnceLock<Mutex<Option<(String, bool, Arc<Regex>)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// 供 UI 在发查询前预编译：非法正则直接提示，不把空结果伪装成“没找到”。
pub fn validate_regex(pattern: &str, case_sensitive: bool) -> Result<(), String> {
    RegexBuilder::new(pattern)
        .case_insensitive(!case_sensitive)
        .build()
        .map(|_| ())
        .map_err(|err| err.to_string())
}

pub(crate) fn compiled_regex(spec: &QuerySpec) -> Option<Arc<Regex>> {
    let pattern = spec.regex.as_deref()?;
    regex_for(pattern, spec.case_sensitive)
}

fn regex_for(pattern: &str, case_sensitive: bool) -> Option<Arc<Regex>> {
    if let Ok(mut cache) = regex_cache().lock() {
        if let Some((cached, case, regex)) = cache.as_ref() {
            if cached == pattern && *case == case_sensitive {
                return Some(Arc::clone(regex));
            }
        }
        let regex = Arc::new(
            RegexBuilder::new(pattern)
                .case_insensitive(!case_sensitive)
                .build()
                .ok()?,
        );
        *cache = Some((pattern.to_string(), case_sensitive, Arc::clone(&regex)));
        return Some(regex);
    }
    None
}

/// 单个名字是否命中一个 token（与文件匹配同语义：ASCII 大小写/词边界）。
/// 应用/命令行的严格匹配复用它，避免拼音/模糊在严格模式下绕过选项。
pub fn name_matches(
    name: &str,
    token: &str,
    case_sensitive: bool,
    whole_word: bool,
) -> bool {
    if token.is_empty() {
        return true;
    }
    if case_sensitive {
        token_matches(name.as_bytes(), token.as_bytes(), whole_word)
    } else {
        token_matches(
            &fold_case(name.as_bytes()),
            &fold_case(token.as_bytes()),
            whole_word,
        )
    }
}

/// 多 token AND（应用/命令行的严格匹配）。
pub fn query_name_matches(
    name: &str,
    query: &str,
    case_sensitive: bool,
    whole_word: bool,
) -> bool {
    query
        .split_whitespace()
        .all(|token| name_matches(name, token, case_sensitive, whole_word))
}

/// 应用/命令行正则匹配（缓存同一条 pattern）。
pub fn regex_name_matches(name: &str, pattern: &str, case_sensitive: bool) -> bool {
    regex_for(pattern, case_sensitive).is_some_and(|regex| regex.is_match(name.as_bytes()))
}

/// 从正则里抽一段「每个匹配都必然包含」的最长字面量（≥2 字节）做 n-gram
/// 预筛；抽不到就全扫（查询可取消）。预筛只是超集，最终仍由 `regex.is_match`
/// 判定，因此宁可少筛（全扫）也不能误筛（假阴性）。
///
/// 旧实现按字节扫「最长字母数字串」，不解析正则结构，会把交替分支 / 字符类 /
/// 转义类里的片段误当必然字面量，造成静默漏结果：`(foo|bar)` 只筛 foo 漏 bar、
/// `[a-z]abc` 抽出 "a-z" 漏 xabc、`\wfoo` 抽出 "wfoo" 漏 xfoo。这里改为解析 HIR
/// 后只取保证出现的字面量（见 [`required_literal`]）。
fn regex_literal(pattern: &str) -> Option<Vec<u8>> {
    // 用默认（大小写敏感）解析提取结构；预筛字面量随后 fold_case 与小写名区
    // 对齐。大小写不敏感匹配的最终判定仍由 compiled_regex 的 case_insensitive
    // 正则完成，故按原样提取字面量再折叠是安全的超集预筛。
    let hir = regex_syntax::Parser::new().parse(pattern).ok()?;
    let lit = required_literal(&hir)?;
    (lit.len() >= 2).then(|| fold_case(&lit))
}

/// 递归提取 HIR 中「必然出现」的最长连续字面量；无保证字面量时返回 None。
/// - Literal：该字面量本身；
/// - Concat：相邻的必然字面量可拼接（全体按序出现），被「无必然字面量」的
///   子项（如字符类）断开，取各连续段中最长者；
/// - Repetition：min≥1 时内部必然出现，否则（可选重复）None；
/// - Capture：递归取捕获组内部；
/// - Alternation：各分支不一定共享字面量，保守返回 None（全扫，绝不漏）；
/// - Class / Look / Empty：零宽或不确定，None。
fn required_literal(hir: &regex_syntax::hir::Hir) -> Option<Vec<u8>> {
    use regex_syntax::hir::HirKind;
    match hir.kind() {
        HirKind::Literal(lit) => Some(lit.0.to_vec()),
        HirKind::Concat(subs) => {
            let mut best: Vec<u8> = Vec::new();
            let mut cur: Vec<u8> = Vec::new();
            for sub in subs {
                match required_literal(sub) {
                    Some(piece) => cur.extend_from_slice(&piece),
                    None => {
                        if cur.len() > best.len() {
                            best = std::mem::take(&mut cur);
                        } else {
                            cur.clear();
                        }
                    }
                }
            }
            if cur.len() > best.len() {
                best = cur;
            }
            (!best.is_empty()).then_some(best)
        }
        HirKind::Repetition(rep) => {
            if rep.min >= 1 {
                required_literal(&rep.sub)
            } else {
                None
            }
        }
        HirKind::Capture(cap) => required_literal(&cap.sub),
        HirKind::Alternation(_) | HirKind::Class(_) | HirKind::Look(_) | HirKind::Empty => None,
    }
}

fn walk_regex(
    store: &VolumeStore,
    spec: &QuerySpec,
    regex: &Regex,
    literal: Option<&[u8]>,
    matcher: Option<&PathMatcher>,
    cancel: Option<u64>,
    emit: &mut impl FnMut(u32) -> bool,
) -> bool {
    let mut consider = |slot: u32| -> bool {
        if !passes_filters(store, slot, spec) {
            return true;
        }
        if matcher.is_some_and(|m| !m.matches(store, slot)) {
            return true;
        }
        if regex.is_match(store.name(slot)) {
            emit(slot)
        } else {
            true
        }
    };
    if let Some(lit) = literal {
        if let Some(candidates) = store.ngram_candidates(lit) {
            for (i, slot) in candidates.into_iter().enumerate() {
                if i & CANCEL_CHECK_MASK == 0 && cancelled(cancel) {
                    return true;
                }
                if !consider(slot) {
                    return false;
                }
            }
            return false;
        }
    }
    for slot in 0..store.name_off.len() as u32 {
        if slot & CANCEL_CHECK_MASK as u32 == 0 && cancelled(cancel) {
            return true;
        }
        if store.attrs[slot as usize] & ATTR_INTERNAL != 0 {
            continue;
        }
        if !consider(slot) {
            return false;
        }
    }
    false
}

fn token_matches(name: &[u8], token: &[u8], whole_word: bool) -> bool {
    if token.is_empty() {
        return true;
    }
    if !whole_word {
        return memchr::memmem::find(name, token).is_some();
    }
    let mut from = 0usize;
    while from + token.len() <= name.len() {
        let Some(rel) = memchr::memmem::find(&name[from..], token) else {
            return false;
        };
        let start = from + rel;
        let end = start + token.len();
        let start_ok = start == 0 || !is_word_byte(name[start - 1]);
        let end_ok = end == name.len() || !is_word_byte(name[end]);
        if start_ok && end_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

/// 路径前缀的祖先段匹配器（无分配，避免为每个候选拼完整路径）。
///
/// 关键点：路径过滤必须在 **候选封顶之前** 生效。旧实现先收集前
/// `candidate_cap` 条全局命中、再用 `path_in_prefix` 后滤；当查询词很常见
/// （`api-` / `dll`）时，前 2000 条可能全在目标目录之外，结果 total 很大、
/// hits 为 0。这里把前缀判断压进 emit，cap/总数都只统计目录内的命中。
///
/// 语义与 [`path_in_prefix`] 一致：候选必须是前缀目录的**后代**（目录自身
/// 不算），路径从卷根开始逐段完全一致。
pub(crate) struct PathMatcher {
    /// 前缀约束（`path:`/深路径）：前缀去掉盘符后的段（小写 ASCII）。
    prefix: Option<Vec<Vec<u8>>>,
    /// 相对路径分段约束（`DeepSeek\xxx`）：末尾连续分段。
    chain: Option<SuffixChain>,
}

/// 相对路径分段约束：组件按输入顺序，从路径**末尾**往前连续匹配。
/// `trailing=false` 时最后一段绑定条目名（`DeepSeek\xxx` 只命中名字含
/// xxx 且父目录含 DeepSeek 的条目）；`trailing=true` 时最后一段绑定父
/// 目录，条目本身可以是它下面任意深度的后代（`DeepSeek\`）。
struct SuffixChain {
    components: Vec<Vec<u8>>,
    trailing: bool,
}

impl SuffixChain {
    fn new(components: &[String], trailing: bool) -> Option<Self> {
        let components: Vec<Vec<u8>> = components
            .iter()
            .filter(|c| !c.is_empty())
            .map(|c| c.as_bytes().to_ascii_lowercase())
            .collect();
        (!components.is_empty()).then_some(Self {
            components,
            trailing,
        })
    }

    /// 从末尾往前逐段比对：每段必须是该级路径名字的**子串**（大小写不敏感，
    /// 与名字搜索一致），不再要求锚定卷根（祖先可以更多）。
    ///
    /// - `trailing=false`（`DeepSeek\xxx`）：严格绑定「非空分段结尾在名字上」，
    ///   即名字含 xxx、父目录含 DeepSeek、祖父含链上一段……
    /// - `trailing=true`（`DeepSeek\`）：链只要出现在祖先链的任意连续位置即可
    ///   （`…\DeepSeek\sub\file` 也命中）。
    fn matches(&self, store: &VolumeStore, slot: u32) -> bool {
        if !self.trailing {
            let mut cur = slot;
            for component in self.components.iter().rev() {
                if cur == NO_PARENT || cur as usize >= store.name_off.len() {
                    return false;
                }
                if memchr::memmem::find(store.name_lc(cur), component).is_none() {
                    return false;
                }
                cur = store.parent[cur as usize];
            }
            return true;
        }
        // 祖先链任意位置连续匹配（与 PathMatcher 同款状态机，换成子串比较）。
        let last = self.components[self.components.len() - 1].as_slice();
        let mut cur = store.parent[slot as usize];
        let mut matched = 0usize;
        let mut hops = 0u32;
        while cur != NO_PARENT && (cur as usize) < store.name_off.len() && hops < 64 {
            let name = store.name_lc(cur);
            let expected = &self.components[self.components.len() - 1 - matched];
            if matched < self.components.len() && memchr::memmem::find(name, expected).is_some() {
                matched += 1;
                if matched == self.components.len() {
                    return true;
                }
            } else if matched > 0 {
                // 断链：当前节点可能重新成为链尾。
                matched = usize::from(memchr::memmem::find(name, last).is_some());
                if matched == self.components.len() {
                    return true;
                }
            }
            cur = store.parent[cur as usize];
            hops += 1;
        }
        false
    }
}

impl PathMatcher {
    /// 只接受本地盘符前缀（`d:\...`）；UNC/相对前缀返回 None，退回后置过滤。
    pub(crate) fn new(prefix: &str) -> Option<Self> {
        let bytes = prefix.trim_end_matches(['\\', '/']).as_bytes();
        if bytes.len() < 2
            || !bytes[0].is_ascii_alphabetic()
            || bytes[1] != b':'
        {
            return None;
        }
        let rest = &prefix.trim_end_matches(['\\', '/'])[2..];
        let segments: Vec<Vec<u8>> = rest
            .split(['\\', '/'])
            .filter(|s| !s.is_empty())
            .map(|s| s.as_bytes().to_ascii_lowercase())
            .collect();
        Some(Self {
            prefix: Some(segments),
            chain: None,
        })
    }

    /// 按 QuerySpec 汇总路径约束：前缀（path:/深路径）与相对分段链可同时存在
    /// （Explorer 目录上下文 + 用户输入 `子目录\文件`），两者都满足才通过。
    pub(crate) fn for_spec(spec: &QuerySpec) -> Option<Self> {
        let prefix = spec
            .path_prefix
            .as_deref()
            .and_then(Self::new)
            .and_then(|matcher| matcher.prefix);
        let chain = (!spec.path_components.is_empty())
            .then(|| SuffixChain::new(&spec.path_components, spec.path_trailing))
            .flatten();
        (prefix.is_some() || chain.is_some()).then_some(Self { prefix, chain })
    }

    /// 是否带有可用的前缀约束（UNC/相对前缀会退化为 None → 引擎走逐条后置过滤）。
    pub(crate) fn has_prefix(&self) -> bool {
        self.prefix.is_some()
    }

    pub(crate) fn matches(&self, store: &VolumeStore, slot: u32) -> bool {
        if let Some(segments) = &self.prefix {
            if !Self::matches_prefix(store, slot, segments) {
                return false;
            }
        }
        if let Some(chain) = &self.chain {
            if !chain.matches(store, slot) {
                return false;
            }
        }
        true
    }

    /// slot 是否位于前缀目录之下。从父链向上按「最后一段→第一段」连续匹配，
    /// 全部匹配后再确认前缀目录是卷根的直接子项（否则只是路径后缀碰巧相同）。
    fn matches_prefix(store: &VolumeStore, slot: u32, segments: &[Vec<u8>]) -> bool {
        if segments.is_empty() {
            return true; // 盘符根：卷内所有条目都在前缀下
        }
        let mut cur = store.parent[slot as usize];
        let mut matched = 0usize;
        let mut hops = 0u32;
        while cur != NO_PARENT && hops < 64 {
            let name = store.name(cur);
            let last = segments[segments.len() - 1].as_slice();
            if matched < segments.len()
                && name.eq_ignore_ascii_case(&segments[segments.len() - 1 - matched])
            {
                matched += 1;
                if matched == segments.len() {
                    return store.parent[cur as usize] == NO_PARENT;
                }
            } else if matched > 0 {
                // 断链：当前节点可能重新成为「最后一段」（避免 a\x\b 误配 a\b）。
                matched = usize::from(name.eq_ignore_ascii_case(last));
                if matched == segments.len() {
                    return store.parent[cur as usize] == NO_PARENT;
                }
            }
            cur = store.parent[cur as usize];
            hops += 1;
        }
        false
    }
}

/// 查询取消代次:UI 每次发出新搜索前写入最新 generation;查询线程在
/// 热循环里每 [`CANCEL_CHECK_EVERY`] 个候选/命中读一次,发现代次变化即
/// 提前中止,避免旧查询(如单字符 "a" 命中 200 万条要 600ms+)把新击键
/// 堵在队列里。一条候选检查一次性原子读,1024 候选 ≈ 10-40µs,
/// 被取代的查询最迟在数百 µs 内中止(远低于 2ms 目标)。
pub static CANCEL_GEN: AtomicU64 = AtomicU64::new(0);

/// 取消检查步长(候选/命中条数)。所有候选遍历路径统一用它:
/// 空 token 枚举、单字符位图、n-gram 候选链、memmem 命中流。
const CANCEL_CHECK_EVERY: usize = 1024;
const CANCEL_CHECK_MASK: usize = CANCEL_CHECK_EVERY - 1;

/// total 精确计数的上限:单字符/高频 token(如 "a")能命中 200 万条,
/// 逐条计数是 600ms 级的成本,而 UI 也只会显示「10万+」。
/// 到达上限即停止计数(收集仍照常封顶 candidate_cap)。
pub const TOTAL_CAP: usize = 100_000;

/// B5:空 token(纯过滤)查询的计数预算。这类查询里所有条目都能通过名字
/// 过滤,逐条计数到 TOTAL_CAP 会让每次筛选变更都近似全卷扫描(实测
/// p50 81ms / max 363ms);而且 Folders 计数几乎撞不到 10 万,split 的
/// 合并遍历会被拖到卷尾。降到 2 万并在扫到该数量候选时直接停扫,
/// total 变为下界——UI 用 `Response.total_rough` 标注「N+」并提示
/// 「筛选较宽,输入关键词可缩小范围」。
pub const FILTER_ONLY_TOTAL_CAP: usize = 20_000;

/// 本次查询的计数预算:B5 的纯过滤查询用更小的 `FILTER_ONLY_TOTAL_CAP`。
fn total_budget_for(spec: &QuerySpec) -> usize {
    if spec.tokens.is_empty() && spec.regex.is_none() {
        FILTER_ONLY_TOTAL_CAP
    } else {
        TOTAL_CAP
    }
}

#[inline]
fn cancelled(cancel: Option<u64>) -> bool {
    matches!(cancel, Some(my) if CANCEL_GEN.load(Ordering::Relaxed) != my)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KindFilter {
    #[default]
    All,
    Files,
    Folders,
    Programs,
}

#[derive(Clone, Debug, Default)]
pub struct QuerySpec {
    /// 原始查询文本 token 序列(provider 按 lstr 语义拆好)。
    pub tokens: Vec<String>,
    pub kind: KindFilter,
    /// 小写扩展名(无点)。
    pub ext: Option<String>,
    /// 通配前缀（小写）：名字必须以它开头；空 prefix 匹配全部。
    pub prefix: Option<String>,
    /// 通配后缀（小写）：名字必须以它结尾。
    pub suffix: Option<String>,
    /// 小写路径前缀(如 `c:\users\x\desktop`);带盘符时引擎只在对应卷查询。
    /// 深路径对收集结果做后置过滤:目录自身不计入(只匹配其后代);
    /// 小结果集(本卷 total 未超 cap)时 total 按过滤后实际条数修正,
    /// 超出 cap 时为近似值。
    pub path_prefix: Option<String>,
    /// 相对路径分段查询（`DeepSeek\xxx`）：按「路径末尾连续分段」匹配。
    /// 为空 = 不是路径分段查询。最后一段（trailing=false）绑定条目名，
    /// 所以 `tokens` 里放的就是它（保持名字锚点与 n-gram 预筛）。
    pub path_components: Vec<String>,
    /// 路径分段查询的末尾是否带分隔符（`DeepSeek\` = 该目录下所有后代）。
    pub path_trailing: bool,
    /// 类型组的扩展名集合(小写、无点)。
    pub group_exts: Vec<String>,
    /// 精确匹配(名字完全一致,小写比较;wfn: 语义)。设置后只返回精确名。
    pub exact: Option<String>,
    /// 修改时间下界(unix 秒,含);None=不限。mtime=0(未知)条目在时间过滤下一律不通过。
    pub mtime_after: Option<i64>,
    /// 修改时间上界(unix 秒,含)。
    pub mtime_before: Option<i64>,
    /// 文件大小下界/上界(字节,含边界)。目录不参与大小过滤；大小未知
    /// (`u64::MAX`)的条目在大小条件下不通过。
    pub size_after: Option<u64>,
    pub size_before: Option<u64>,
    /// 卷掩码(bit i='A'+i):None=不限制,Some(mask)=只在这些卷查询
    /// (mask 为 0 表示排除掉了所有卷,查询结果为空)。
    pub vol_mask: Option<u32>,
    /// 区分 ASCII 大小写（默认 false = 现有大小写不敏感语义）。
    pub case_sensitive: bool,
    /// 全词匹配：每个 token 前后必须是词边界（ASCII 字母数字为词字符）。
    pub whole_word: bool,
    /// 正则模式：整个输入作为 bytes 正则；设置后忽略 tokens。
    pub regex: Option<String>,
}

/// 盘符字母 → 卷掩码位(bit i = 'A'+i)。非字母返回 0。
pub fn volume_bit(letter: char) -> u32 {
    let upper = letter.to_ascii_uppercase();
    if upper.is_ascii_uppercase() {
        1u32 << (upper as u32 - 'A' as u32)
    } else {
        0
    }
}

/// 深路径前缀匹配。`path`/`prefix` 按调用约定均已小写。
///
/// - 目录自身(path == prefix)**不算**:`path:E:\dir` 只返回 dir 的后代,
///   不把 dir 自己列出来;
/// - prefix 末尾是分隔符时直接匹配其后所有内容(`e:\dir\`);
/// - 否则要求 prefix 后紧跟 `\` 或 `/` —— 同前缀兄弟目录不能被误命中
///   (`e:\dir2` 不匹配 `e:\dir`)。
///
/// 空 prefix 视为不限制(旧行为:所有路径 starts_with("") 都为真)。
pub fn path_in_prefix(path: &str, prefix: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    path.len() > prefix.len()
        && path.starts_with(prefix)
        && (prefix.ends_with('\\')
            || prefix.ends_with('/')
            || matches!(path.as_bytes()[prefix.len()], b'\\' | b'/'))
}

impl QuerySpec {
    pub fn is_empty_text(&self) -> bool {
        self.tokens.iter().all(|t| t.is_empty())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hit {
    pub vol: u32,
    pub slot: u32,
    /// 卷索引代次:compact/重扫/重置/卷移除后校验,拒绝过期命中(H8)。
    pub generation: u32,
    /// 索引里的 mtime(unix 秒,0 = 未知)。候选排序(最近修改加权)直接
    /// 复用该值,零 FS 调用;展示条目的 mtime 仍由 finalize 从 FS 重新取。
    pub mtime: i64,
}

#[derive(Clone, Debug, Default)]
pub struct QueryOutcome {
    pub hits: Vec<Hit>,
    pub total: usize,
    /// 因 candidate_cap 提前截断(total 仍精确,仅收集截断)。
    pub capped: bool,
    /// 新查询已到达,本次查询被提前中止(hits/total 均为部分结果,调用方应丢弃)。
    pub aborted: bool,
}

/// 判断 slot 是否通过 kind/ext/组 过滤(不含路径)。
pub fn passes_filters(store: &VolumeStore, slot: u32, spec: &QuerySpec) -> bool {
    let attrs = store.attrs[slot as usize];
    if attrs & ATTR_INTERNAL != 0 {
        return false;
    }
    if spec.mtime_after.is_some() || spec.mtime_before.is_some() {
        let m = store.mtime[slot as usize] as i64;
        if m == 0 {
            return false;
        }
        if let Some(after) = spec.mtime_after {
            if m < after {
                return false;
            }
        }
        if let Some(before) = spec.mtime_before {
            if m > before {
                return false;
            }
        }
    }
    let is_dir = attrs & FILE_ATTRIBUTE_DIRECTORY_U16 != 0;
    if spec.size_after.is_some() || spec.size_before.is_some() {
        // 目录没有有意义的大小；大小未知（USN 增量尚未回填）一律不通过，
        // 与 mtime=0 在时间条件下被排除的口径一致。
        if is_dir {
            return false;
        }
        let size = store.size[slot as usize];
        if size == u64::MAX {
            return false;
        }
        if let Some(after) = spec.size_after {
            if size < after {
                return false;
            }
        }
        if let Some(before) = spec.size_before {
            if size > before {
                return false;
            }
        }
    }
    // 快路径:All 且无 ext/组过滤时无需看名字;Folders 同理(目录判定只看
    // attrs)。单字符 "a" 要扫 290 万 slot,旧实现每条都做 name_lc + 后缀
    // 比较,是 folders 路 39ms 的主因;这里让绝大多数候选只读 attrs。
    let need_name = spec.ext.is_some()
        || spec.prefix.is_some()
        || spec.suffix.is_some()
        || !spec.group_exts.is_empty()
        || matches!(spec.kind, KindFilter::Files | KindFilter::Programs);
    if !need_name {
        return match spec.kind {
            KindFilter::All => true,
            KindFilter::Folders => is_dir,
            KindFilter::Files | KindFilter::Programs => unreachable!(),
        };
    }
    let name = store.name_lc(slot);
    if let Some(prefix) = &spec.prefix {
        if !name.starts_with(prefix.as_bytes()) {
            return false;
        }
    }
    if let Some(suffix) = &spec.suffix {
        if !name.ends_with(suffix.as_bytes()) {
            return false;
        }
    }
    // 只有 Files/Programs 需要 .exe 判定;All/Folders 不再无谓计算。
    let is_exe = matches!(spec.kind, KindFilter::Files | KindFilter::Programs)
        && name.ends_with(b".exe");
    let kind_ok = match spec.kind {
        KindFilter::All => true,
        KindFilter::Folders => is_dir,
        KindFilter::Files => !is_dir && !is_exe,
        KindFilter::Programs => is_exe,
    };
    if !kind_ok {
        return false;
    }
    let ext_ok = |ext: &str| {
        name.len() > ext.len() + 1
            && name.ends_with(ext.as_bytes())
            && name[name.len() - ext.len() - 1] == b'.'
    };
    if let Some(ext) = &spec.ext {
        if !ext_ok(ext) {
            return false;
        }
    }
    if !spec.group_exts.is_empty() && !spec.group_exts.iter().any(|e| ext_ok(e)) {
        return false;
    }
    true
}

/// 单卷候选收集。返回 (slots, total, capped)。
pub fn query_volume(store: &VolumeStore, spec: &QuerySpec, cap: usize) -> (Vec<u32>, usize, bool) {
    let (slots, total, capped, _) = query_volume_cancel(store, spec, cap, None);
    (slots, total, capped)
}

/// 归一化 token(去空、小写折叠),各查询路径共用。
fn query_tokens(spec: &QuerySpec) -> Vec<Vec<u8>> {
    spec.tokens
        .iter()
        .filter(|t| !t.is_empty())
        .map(|t| fold_case(t.as_bytes()))
        .collect()
}

/// exact 模式候选集:ready 走 offs 二分、scan 全扫,顺序/过滤与
/// `walk_candidates` 的 exact 分支逐条一致。旧 exact 查询不受 cap/TOTAL_CAP
/// 限制,这里同样收集全部同名槽位,供 `query_split_with_exact` 并趟复用。
pub(crate) fn exact_slots(store: &VolumeStore, spec: &QuerySpec, ex: &[u8]) -> Vec<u32> {
    let mut slots = Vec::new();
    // offs 按小写名排序；大小写敏感时先用折叠名定位区间，再按原始名判等。
    let folded = if spec.case_sensitive {
        fold_case(ex)
    } else {
        ex.to_vec()
    };
    if !store.offs.is_empty() {
        let lo = store.offs.partition_point(|&s| store.name_lc(s) < &folded[..]);
        for &slot in &store.offs[lo..] {
            if store.name_lc(slot) != folded {
                break;
            }
            let exact_ok = !spec.case_sensitive || store.name(slot) == ex;
            if exact_ok && passes_filters(store, slot, spec) {
                slots.push(slot);
            }
        }
        return slots;
    }
    for slot in 0..store.name_off.len() as u32 {
        let exact_ok = if spec.case_sensitive {
            store.name(slot) == ex
        } else {
            store.name_lc(slot) == ex
        };
        if exact_ok && passes_filters(store, slot, spec) {
            slots.push(slot);
        }
    }
    slots
}

/// 前缀候选集(A-3):名字以 prefix 开头的槽位,ready 走 offs 二分、scan 全扫。
/// 宽查询的子串候选按名字序封顶 candidate_cap,前缀命中(排序仅次于精确同名)
/// 可能被字母序靠前的海量子串命中挤出候选窗。此函数供「前缀补集」用二分
/// 直接拿前缀区间(成本 ~µs),保证前缀命中总能进入排序。cap 限制收集量。
pub(crate) fn prefix_slots(
    store: &VolumeStore,
    spec: &QuerySpec,
    prefix: &[u8],
    cap: usize,
) -> Vec<u32> {
    let mut slots = Vec::new();
    let folded = if spec.case_sensitive {
        fold_case(prefix)
    } else {
        prefix.to_vec()
    };
    if folded.is_empty() || cap == 0 {
        return slots;
    }
    if !store.offs.is_empty() {
        let lo = store.offs.partition_point(|&s| store.name_lc(s) < &folded[..]);
        for &slot in &store.offs[lo..] {
            if slots.len() >= cap {
                break;
            }
            // 小写名序里前缀区间连续;一旦不再以 folded 开头即可停止。
            if !store.name_lc(slot).starts_with(&folded[..]) {
                break;
            }
            let ok = !spec.case_sensitive || store.name(slot).starts_with(prefix);
            if ok && passes_filters(store, slot, spec) {
                slots.push(slot);
            }
        }
        return slots;
    }
    for slot in 0..store.name_off.len() as u32 {
        if slots.len() >= cap {
            break;
        }
        let ok = if spec.case_sensitive {
            store.name(slot).starts_with(prefix)
        } else {
            store.name_lc(slot).starts_with(&folded[..])
        };
        if ok && passes_filters(store, slot, spec) {
            slots.push(slot);
        }
    }
    slots
}

/// 可取消版本:返回 `(slots, total, capped, aborted)`。查询线程传入发查询时
/// 的 `CANCEL_GEN` 快照,UI 每次新搜索都会刷新该代次,热循环据此提前中止。
pub(crate) fn query_volume_cancel(
    store: &VolumeStore,
    spec: &QuerySpec,
    cap: usize,
    cancel: Option<u64>,
) -> (Vec<u32>, usize, bool, bool) {
    let matcher = PathMatcher::for_spec(spec);
    if let Some(pattern) = spec.regex.as_deref() {
        let Some(regex) = compiled_regex(spec) else {
            return (Vec::new(), 0, false, false);
        };
        let literal = regex_literal(pattern);
        let mut slots: Vec<u32> = Vec::with_capacity(256);
        let mut total = 0usize;
        let mut capped = false;
        let aborted = walk_regex(
            store,
            spec,
            &regex,
            literal.as_deref(),
            matcher.as_ref(),
            cancel,
            &mut |slot| {
                total += 1;
                if total >= TOTAL_CAP {
                    return false;
                }
                if slots.len() < cap {
                    slots.push(slot);
                } else {
                    capped = true;
                }
                true
            },
        );
        return (slots, total, capped, aborted);
    }
    let tokens = query_tokens(spec);
    let match_matcher = MatchMatcher::new(spec);
    // 精确模式沿用旧语义:收集与 total 都不受 cap/TOTAL_CAP 限制(名字完全
    // 一致的条目本来就少,旧实现直接返回全部;wfn 补齐依赖这个行为)。
    let unbounded = !tokens.is_empty() && spec.exact.is_some();
    let cap_eff = if unbounded { usize::MAX } else { cap };
    // B5:纯过滤查询(空 token)用更小的计数预算,避免每次筛选变更都数到
    // TOTAL_CAP 才停;total 变为下界,由调用方用 Response.total_rough 标注。
    let total_budget = total_budget_for(spec);
    let mut slots: Vec<u32> = Vec::with_capacity(256);
    let mut total = 0usize;
    let mut capped = false;
    let aborted = walk_candidates(
        store,
        spec,
        &tokens,
        matcher.as_ref(),
        match_matcher.as_ref(),
        cancel,
        &mut |slot| {
        total += 1;
        if !unbounded && total >= total_budget {
            return false; // total 已到显示上限,停止扫描(收集也随之中止)
        }
        if slots.len() < cap_eff {
            slots.push(slot);
        } else {
            capped = true;
        }
        true
    });
    (slots, total, capped, aborted)
}

/// 单卷「文件夹 + 全部」一次遍历收集。一次候选遍历同时产出:
/// - `all_slots`:All-kind 命中的前 `all_cap` 条(含文件夹),对应旧 files 路
///   的引擎候选前缀(应用层再滤掉文件夹并截断);
/// - `folder_slots`:Folders 命中的前 `folder_cap` 条,对应旧 folders 路;
/// - `total_all`:All-kind 精确计数(TOTAL_CAP 封顶);
/// - `total_folders`:Folders 精确计数(TOTAL_CAP 封顶)。
/// 两个 total 各自独立封顶,与两次独立查询逐字节一致;旧实现要跑两遍引擎,
/// 现在候选生成/过滤只跑一遍。
pub(crate) fn query_volume_split_cancel(
    store: &VolumeStore,
    spec: &QuerySpec,
    folder_cap: usize,
    all_cap: usize,
    cancel: Option<u64>,
) -> (Vec<u32>, Vec<u32>, usize, usize, bool) {
    debug_assert!(spec.exact.is_none(), "split 查询不使用 exact 模式");
    let matcher = PathMatcher::for_spec(spec);
    if let Some(pattern) = spec.regex.as_deref() {
        let Some(regex) = compiled_regex(spec) else {
            return (Vec::new(), Vec::new(), 0, 0, false);
        };
        let literal = regex_literal(pattern);
        let mut all_slots: Vec<u32> = Vec::with_capacity(256);
        let mut folder_slots: Vec<u32> = Vec::with_capacity(256);
        let mut total_all = 0usize;
        let mut total_folders = 0usize;
        let mut all_done = false;
        let mut folder_done = false;
        let aborted = walk_regex(
            store,
            spec,
            &regex,
            literal.as_deref(),
            matcher.as_ref(),
            cancel,
            &mut |slot| {
                if all_done && folder_done {
                    return false;
                }
                let is_dir = store.is_dir(slot);
                if !all_done {
                    total_all += 1;
                    if total_all >= TOTAL_CAP {
                        all_done = true;
                    } else if all_slots.len() < all_cap {
                        all_slots.push(slot);
                    }
                }
                if is_dir && !folder_done {
                    total_folders += 1;
                    if total_folders >= TOTAL_CAP {
                        folder_done = true;
                    } else if folder_slots.len() < folder_cap {
                        folder_slots.push(slot);
                    }
                }
                !(all_done && folder_done)
            },
        );
        return (all_slots, folder_slots, total_all, total_folders, aborted);
    }
    let tokens = query_tokens(spec);
    let match_matcher = MatchMatcher::new(spec);
    // B5:纯过滤查询(空 token)把计数预算降到 FILTER_ONLY_TOTAL_CAP,并在
    // 扫到同样数量的候选后直接停——否则 Folders 计数几乎撞不到 10 万,
    // 合并遍历会被拖到卷尾(等价于全卷扫描)。total 变为下界,由调用方用
    // Response.total_rough 提示「N+」。
    let total_budget = total_budget_for(spec);
    let filter_only = total_budget < TOTAL_CAP;
    let mut seen = 0usize;
    let mut all_slots: Vec<u32> = Vec::with_capacity(256);
    let mut folder_slots: Vec<u32> = Vec::with_capacity(256);
    let mut total_all = 0usize;
    let mut total_folders = 0usize;
    let mut all_done = false;
    let mut folder_done = false;
    let aborted = walk_candidates(
        store,
        spec,
        &tokens,
        matcher.as_ref(),
        match_matcher.as_ref(),
        cancel,
        &mut |slot| {
        if all_done && folder_done {
            return false;
        }
        if filter_only {
            if seen >= total_budget {
                return false; // B5:够数即停,不再为精确 total 扫全卷
            }
            seen += 1;
        }
        let is_dir = store.is_dir(slot);
        if !all_done {
            total_all += 1;
            if total_all >= total_budget {
                all_done = true; // 与旧 All 查询一致:第 total_budget 条只计数不收集
            } else if all_slots.len() < all_cap {
                all_slots.push(slot);
            }
        }
        if is_dir && !folder_done {
            total_folders += 1;
            if total_folders >= total_budget {
                folder_done = true;
            } else if folder_slots.len() < folder_cap {
                folder_slots.push(slot);
            }
        }
        !(all_done && folder_done)
    });
    (all_slots, folder_slots, total_all, total_folders, aborted)
}

/// 候选遍历内核:按 [`QuerySpec`] 的同一套语义(空 token 枚举 / 精确 / 单字符
/// 位图 / n-gram / memmem)产出通过过滤的 slot,顺序与旧实现完全一致。
/// `emit` 返回 false 表示调用方已集齐、停止扫描;返回值 = 是否被新查询中止。
fn walk_candidates(
    store: &VolumeStore,
    spec: &QuerySpec,
    tokens: &[Vec<u8>],
    matcher: Option<&PathMatcher>,
    match_matcher: Option<&MatchMatcher>,
    cancel: Option<u64>,
    emit_inner: &mut impl FnMut(u32) -> bool,
) -> bool {
    // 路径前缀 + 严格匹配在 emit 前过滤：候选封顶与 total 都只统计
    // 目录内、且满足大小写/全词语义的命中。
    let mut emit_path = |slot: u32| {
        if matcher.is_some_and(|m| !m.matches(store, slot)) {
            return true;
        }
        if match_matcher.is_some_and(|m| !m.matches(store, slot)) {
            return true;
        }
        emit_inner(slot)
    };
    let emit = &mut emit_path;
    if tokens.is_empty() {
        // 通配前缀/后缀（`abc*` / `*abc`）：先用字面量走 n-gram 预筛，
        // 再由 passes_filters 做 starts_with/ends_with 严格校验。
        if let Some(literal) = spec
            .prefix
            .as_deref()
            .or(spec.suffix.as_deref())
            .filter(|literal| !literal.is_empty())
        {
            if let Some(candidates) =
                store.ngram_candidates(&fold_case(literal.as_bytes()))
            {
                for (i, slot) in candidates.into_iter().enumerate() {
                    if i & CANCEL_CHECK_MASK == 0 && cancelled(cancel) {
                        return true;
                    }
                    if store.attrs[slot as usize] & ATTR_INTERNAL != 0 {
                        continue;
                    }
                    if !passes_filters(store, slot, spec) {
                        continue;
                    }
                    if !emit(slot) {
                        return false;
                    }
                }
                return false;
            }
        }
        // H5:空 token(单独 ext:rs / path:C:\dir 等过滤器查询)按过滤器全量列举,
        // 逐 slot 走 passes_filters(名字/类型/扩展名/组),收集封顶 candidate_cap,
        // total 精确统计。旧实现在此直接返回空 → 内置 Ready 后这两类筛选静默 0 结果。
        for slot in 0..store.name_off.len() as u32 {
            if slot & CANCEL_CHECK_MASK as u32 == 0 && cancelled(cancel) {
                return true;
            }
            if store.attrs[slot as usize] & ATTR_INTERNAL != 0 {
                continue;
            }
            if !passes_filters(store, slot, spec) {
                continue;
            }
            if !emit(slot) {
                break;
            }
        }
        return false;
    }
    // 精确模式:wfn 语义,直接二分 offs(ready 模式);scan 模式退化为逐项比对
    if cancelled(cancel) {
        return true;
    }
    if let Some(ex) = &spec.exact {
        let exb = ex.as_bytes();
        // offs 按小写名排序；大小写敏感时先用折叠名定位区间，再用原始名判等。
        let folded = if spec.case_sensitive {
            fold_case(exb)
        } else {
            exb.to_vec()
        };
        if !store.offs.is_empty() {
            let lo = store.offs.partition_point(|&s| store.name_lc(s) < &folded[..]);
            for &slot in &store.offs[lo..] {
                if store.name_lc(slot) != folded {
                    break;
                }
                let exact_ok = !spec.case_sensitive || store.name(slot) == exb;
                if exact_ok && passes_filters(store, slot, spec) && !emit(slot) {
                    return false;
                }
            }
            return false;
        }
        for slot in 0..store.name_off.len() as u32 {
            let exact_ok = if spec.case_sensitive {
                store.name(slot) == exb
            } else {
                store.name_lc(slot) == exb
            };
            if exact_ok && passes_filters(store, slot, spec) && !emit(slot) {
                return false;
            }
        }
        return false;
    }

    // E:单字符 ASCII 查询(打字过程中的临时串)直接用名字字母位图过滤,
    // 否则要对整卷 names_lc 做 memmem + 每个命中二分,20 万~200 万命中就是数百 ms。
    if tokens.len() == 1 && tokens[0].len() == 1 && tokens[0][0].is_ascii_alphabetic() {
        let bit = 1u32 << (tokens[0][0] - b'a');
        for slot in 0..store.name_off.len() as u32 {
            if slot & CANCEL_CHECK_MASK as u32 == 0 && cancelled(cancel) {
                return true;
            }
            if store.char_mask[slot as usize] & bit == 0 {
                continue;
            }
            if !passes_filters(store, slot, spec) {
                continue;
            }
            if !emit(slot) {
                break;
            }
        }
        return false;
    }

    // 生成 token 取最长(一般最稀有)
    let gen_idx = tokens
        .iter()
        .enumerate()
        .max_by_key(|(_, t)| t.len())
        .map(|(i, _)| i)
        .unwrap_or(0);
    let gen = &tokens[gen_idx];

    // N:n-gram 倒排快路径。2/3 字符走单条精确链,≥4 字符 trigram 链求交后
    // memmem 验证;候选序(slot 升序 ≡ name_off 序)与 memmem 偏移序一致,
    // 结果与全量扫描路径完全相同(同序同分),只是省掉全量扫描与逐命中二分。
    if let Some(base) = store.ngram_candidates(gen) {
        return walk_ngram(store, spec, tokens, gen_idx, gen, base, cancel, emit);
    }
    walk_memmem(store, spec, tokens, gen_idx, gen, cancel, emit)
}

/// n-gram 快路径遍历。候选序 = base 链(slot 升序)++ overlay(name_off 序):
/// compact 后 slot 序 ≡ name_off 序;overlay 槽(新增/改名)的名字区偏移恒大于
/// 构建期偏移,故拼接后仍是全局偏移序 —— 与 memmem 路径的输出同序同分。
fn walk_ngram(
    store: &VolumeStore,
    spec: &QuerySpec,
    tokens: &[Vec<u8>],
    gen_idx: usize,
    gen: &[u8],
    mut cands: Vec<u32>,
    cancel: Option<u64>,
    emit: &mut impl FnMut(u32) -> bool,
) -> bool {
    // overlay:倒排构建后的新增/改名 slot 逐个验证(不在倒排里,或旧名
    // posting 已失效)。字母位图先粗筛(子串出现 ⇒ 名字含 gen 的全部字母),
    // 再 memmem 验证;renames>0 时 name_off 跳到名字区尾,按偏移重排。
    let re = store.reindex_slots();
    if !re.is_empty() {
        let gen_mask = crate::store::mask_of_bytes(gen);
        let mut ov: Vec<u32> = Vec::new();
        for (j, &slot) in re.iter().enumerate() {
            if j & CANCEL_CHECK_MASK == CANCEL_CHECK_MASK && cancelled(cancel) {
                return true;
            }
            if store.attrs[slot as usize] & ATTR_INTERNAL != 0 {
                continue;
            }
            if store.char_mask[slot as usize] & gen_mask != gen_mask {
                continue;
            }
            if memchr::memmem::find(store.name_lc(slot), gen).is_some() {
                ov.push(slot);
            }
        }
        // 排序本身不可中断:入场前先看一次代次;改名风暴 overlay 最大 ~8 万,
        // sort 是这条路径上唯一的非循环段(实测 ~1-3ms)。
        if cancelled(cancel) {
            return true;
        }
        if store.has_pending_rename() {
            ov.sort_unstable_by_key(|&s| store.name_off[s as usize]);
        }
        if cancelled(cancel) {
            return true;
        }
        // base 里被改名的槽是旧名假阳性,剔除(新名由 overlay 覆盖)
        cands.retain(|&s| !store.base_posting_stale(s));
        cands.extend(ov);
    }
    // ≥4 字符候选是 trigram 交集超集,须逐个验证 gen 真实出现;
    // 2/3 字符链是精确命中集,免验证。
    let verify_gen = gen.len() >= 4;
    for (i, &slot) in cands.iter().enumerate() {
        if i & CANCEL_CHECK_MASK == CANCEL_CHECK_MASK && cancelled(cancel) {
            return true;
        }
        let name = store.name_lc(slot);
        if verify_gen && memchr::memmem::find(name, gen).is_none() {
            continue;
        }
        // 其余 token AND(与 memmem 路径同语义)
        if !tokens
            .iter()
            .enumerate()
            .all(|(j, t)| j == gen_idx || memchr::memmem::find(name, t).is_some())
        {
            continue;
        }
        if !passes_filters(store, slot, spec) {
            continue;
        }
        if !emit(slot) {
            return false;
        }
    }
    false
}

/// memmem 全量扫描遍历(倒排不可用时兜底;等价性测试用独立实现做 oracle)。
fn walk_memmem(
    store: &VolumeStore,
    spec: &QuerySpec,
    tokens: &[Vec<u8>],
    gen_idx: usize,
    gen: &[u8],
    cancel: Option<u64>,
    emit: &mut impl FnMut(u32) -> bool,
) -> bool {
    let finder = memchr::memmem::Finder::new(gen);
    let mut seen = 0usize;
    let mut last_slot: Option<u32> = None; // 同一条目内多次命中只算一次
    for off in finder.find_iter(&store.names_lc) {
        seen += 1;
        if seen & CANCEL_CHECK_MASK == 0 && cancelled(cancel) {
            return true;
        }
        // 命中偏移 → slot(off_index 二分 + 陈项过滤;就地改名的幽灵命中在此被丢弃)
        let Some(slot) = store.slot_at_offset(off) else {
            continue;
        };
        let start = store.name_off[slot as usize] as usize;
        let end = start + store.name(slot).len();
        if off + gen.len() > end {
            continue; // 越界兜底(\0 已保证不会发生)
        }
        if last_slot == Some(slot) {
            continue;
        }
        last_slot = Some(slot);
        // 其余 token AND
        let name = store.name_lc(slot);
        if !tokens
            .iter()
            .enumerate()
            .all(|(i, t)| i == gen_idx || memchr::memmem::find(name, t).is_some())
        {
            continue;
        }
        if !passes_filters(store, slot, spec) {
            continue;
        }
        if !emit(slot) {
            return false;
        }
    }
    false
}

/// memmem 全量扫描路径(倒排不可用时兜底;等价性测试用它对拍快路径)。
/// 语义:按 names_lc 偏移序枚举 gen 的全部命中 → slot 解析(丢弃陈项)→
/// 同条目去重 → 其余 token AND → 过滤 → total 精确计数(TOTAL_CAP 封顶)、
/// 收集封顶 cap。
#[cfg(test)]
pub(crate) fn query_volume_memmem_scan(
    store: &VolumeStore,
    spec: &QuerySpec,
    tokens: &[Vec<u8>],
    gen_idx: usize,
    gen: &[u8],
    cap: usize,
    cancel: Option<u64>,
) -> (Vec<u32>, usize, bool, bool) {
    let mut slots: Vec<u32> = Vec::with_capacity(256);
    let mut total = 0usize;
    let mut capped = false;
    let mut last_slot: Option<u32> = None; // 同一条目内多次命中只算一次
    let matcher = PathMatcher::for_spec(spec);

    let finder = memchr::memmem::Finder::new(gen);
    let mut seen = 0usize;
    for off in finder.find_iter(&store.names_lc) {
        seen += 1;
        if seen & CANCEL_CHECK_MASK == 0 && cancelled(cancel) {
            return (slots, total, capped, true);
        }
        // 命中偏移 → slot(off_index 二分 + 陈项过滤;就地改名的幽灵命中在此被丢弃)
        let Some(slot) = store.slot_at_offset(off) else {
            continue;
        };
        let start = store.name_off[slot as usize] as usize;
        let end = start + store.name(slot).len();
        if off + gen.len() > end {
            continue; // 越界兜底(\0 已保证不会发生)
        }
        if last_slot == Some(slot) {
            continue;
        }
        last_slot = Some(slot);
        // 其余 token AND
        let name = store.name_lc(slot);
        if !tokens
            .iter()
            .enumerate()
            .all(|(i, t)| i == gen_idx || memchr::memmem::find(name, t).is_some())
        {
            continue;
        }
        if !passes_filters(store, slot, spec) {
            continue;
        }
        if matcher.as_ref().is_some_and(|m| !m.matches(store, slot)) {
            continue;
        }
        total += 1;
        if total >= TOTAL_CAP {
            break;
        }
        if slots.len() < cap {
            slots.push(slot);
        } else {
            capped = true;
        }
    }
    (slots, total, capped, false)
}

/// 拼音候选匹配:全部 token 命中 initials 或 full。前缀任意长度可用
/// (首字母 w → wx/weixin);子串需 ≥2 字符(A-5)——单字符做子串会命中
/// 大量拼音中间含该字母的弱相关名字(350 档噪声),只保留其前缀语义。
pub fn pinyin_match(initials: &[u8], full: &[u8], tokens: &[Vec<u8>]) -> bool {
    tokens.iter().all(|t| {
        initials.starts_with(t)
            || full.starts_with(t)
            || (t.len() >= 2
                && (memchr::memmem::find(initials, t).is_some()
                    || memchr::memmem::find(full, t).is_some()))
    })
}

#[cfg(test)]
mod pinyin_match_tests {
    use super::pinyin_match;

    #[test]
    fn single_char_prefix_ok_substring_rejected() {
        // w → 微信(initials wx / full weixin) 前缀命中保留
        assert!(pinyin_match(b"wx", b"weixin", &[b"w".to_vec()]));
        // 单字符子串(拼音中间含 w)拒绝:如「文件」wj 不含 w 前缀但... 用反例
        // 「啊」a:initials a / full a,w 不是其前缀也不应子串命中
        assert!(!pinyin_match(b"a", b"a", &[b"w".to_vec()]));
        // 「安全」anquan:full 中间含 w? 不含。用「中文」zhongwen:full 含 w 但非前缀
        assert!(!pinyin_match(b"zw", b"zhongwen", &[b"w".to_vec()]), "单字符不做子串");
    }

    #[test]
    fn multi_char_substring_still_works() {
        // ei → weixin 子串命中(≥2)保留
        assert!(pinyin_match(b"wx", b"weixin", &[b"ei".to_vec()]));
        assert!(pinyin_match(b"bgdocx", b"baogaodocx", &[b"gao".to_vec()]));
        // 多 token AND
        assert!(pinyin_match(b"wx", b"weixin", &[b"w".to_vec(), b"ei".to_vec()]));
        assert!(!pinyin_match(b"wx", b"weixin", &[b"w".to_vec(), b"zz".to_vec()]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::VolumeStore;

    /// 深路径前缀:目录自身不算命中;同前缀兄弟目录不误命中;末尾分隔符
    /// 与正/反斜杠后代都按预期。
    #[test]
    fn path_in_prefix_excludes_directory_itself_and_siblings() {
        let prefix = r"e:\data\lstracc-文档";
        // 目录自身:path == prefix → 不算
        assert!(!path_in_prefix(prefix, prefix));
        // 后代(反斜杠/正斜杠)命中
        assert!(path_in_prefix(r"e:\data\lstracc-文档\a.txt", prefix));
        assert!(path_in_prefix(r"e:\data\lstracc-文档/sub", prefix));
        // 同前缀兄弟目录及其子项不得命中(前缀后必须紧跟分隔符)
        assert!(!path_in_prefix(r"e:\data\lstracc-文档2", prefix));
        assert!(!path_in_prefix(r"e:\data\lstracc-文档2\a.txt", prefix));
        // prefix 自带末尾分隔符:直接匹配其后所有内容
        let with_sep = r"e:\data\lstracc-文档\";
        assert!(path_in_prefix(r"e:\data\lstracc-文档\a.txt", with_sep));
        assert!(!path_in_prefix(r"e:\data\lstracc-文档2\a.txt", with_sep));
        // 空 prefix 保持旧语义(不限制)
        assert!(path_in_prefix(r"e:\anything", ""));
        // 完全不同的路径
        assert!(!path_in_prefix(r"e:\other\a.txt", prefix));
    }

    /// 回归（2026-11-11 `api-` 目录内搜索 0 结果）：路径前缀必须在候选封顶
    /// **之前**过滤。旧实现先收前 2000 条全局命中再后滤路径，目标目录外的
    /// 常见词命中把 cap 占满，目录内的条目一条都进不了 hits。
    #[test]
    fn path_prefix_filters_before_candidate_cap() {
        let mut s = VolumeStore::new(Default::default(), 'C');
        // 卷根的直接子目录（生产快照里顶层条目 parent 就是 NO_PARENT）
        s.insert(10, 0, 0x10, b"target");
        s.insert(11, 0, 0x10, b"other");
        for i in 0..3000u64 {
            s.insert(1000 + i, 11, 0, format!("api-{i}.dll").as_bytes());
        }
        s.insert(9000, 10, 0, b"api-target.dll");
        s.compact();

        let spec = QuerySpec {
            tokens: vec!["api-".into()],
            path_prefix: Some(r"d:\target".into()),
            ..Default::default()
        };
        let (slots, total, _, _) = query_volume_cancel(&s, &spec, 2000, None);
        assert_eq!(total, 1, "total 只应统计目标目录内的命中");
        assert_eq!(slots.len(), 1, "目标目录内的条目不能被全局 cap 挤掉");
        assert_eq!(s.name(slots[0]), b"api-target.dll");
    }

    /// 前缀必须从卷根开始逐段连续匹配：`b\a\b\file` 不能命中 `a\b`。
    #[test]
    fn path_matcher_anchors_at_volume_root() {
        let mut s = VolumeStore::new(Default::default(), 'C');
        s.insert(10, 0, 0x10, b"b");
        s.insert(11, 10, 0x10, b"a");
        s.insert(12, 11, 0x10, b"b");
        s.insert(13, 12, 0, b"file.txt");
        s.compact();

        let matcher = PathMatcher::new(r"d:\a\b").expect("盘符前缀");
        assert!(!matcher.matches(&s, s.find_slot(13).unwrap()), "后缀碰巧相同不算");

        // 真正的 a\b\file 命中，目录自身不计。
        let mut ok = VolumeStore::new(Default::default(), 'C');
        ok.insert(20, 0, 0x10, b"a");
        ok.insert(21, 20, 0x10, b"b");
        ok.insert(22, 21, 0, b"file.txt");
        ok.compact();
        let matcher = PathMatcher::new(r"d:\a\b").expect("盘符前缀");
        assert!(matcher.matches(&ok, ok.find_slot(22).unwrap()));
        assert!(!matcher.matches(&ok, ok.find_slot(21).unwrap()), "前缀目录自身不算");
    }

    /// `e:\aa\aa\DeepSeek\xxx` 这类结构，用于相对路径分段测试。
    fn path_chain_store() -> VolumeStore {
        let mut s = VolumeStore::new(Default::default(), 'E');
        s.insert(10, 0, 0x10, b"aa");
        s.insert(11, 10, 0x10, b"aa");
        s.insert(12, 11, 0x10, b"DeepSeek");
        s.insert(13, 12, 0x10, b"xxx");
        s.insert(14, 13, 0, b"report.txt");
        s.insert(15, 12, 0, b"yyy.txt");
        s.insert(20, 0, 0x10, b"other");
        s.insert(21, 20, 0x10, b"DeepSeek");
        s.insert(22, 21, 0x10, b"zzz");
        s.compact();
        s
    }

    fn chain_spec(components: &[&str], trailing: bool) -> QuerySpec {
        QuerySpec {
            // 不带尾分隔符时最后一段绑定名字 → 名字锚点就是它。
            tokens: if trailing {
                Vec::new()
            } else {
                vec![components[components.len() - 1].to_string()]
            },
            path_components: components.iter().map(|c| c.to_string()).collect(),
            path_trailing: trailing,
            ..Default::default()
        }
    }

    fn names_of(store: &VolumeStore, slots: &[u32]) -> Vec<String> {
        let mut names: Vec<String> = slots
            .iter()
            .map(|&slot| String::from_utf8_lossy(store.name(slot)).into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn path_chain_binds_last_component_to_name() {
        let s = path_chain_store();
        // DeepSeek\xxx：名字含 xxx，且父目录含 DeepSeek（可在任意祖先层）。
        let (slots, total, _) = query_volume(&s, &chain_spec(&["DeepSeek", "xxx"], false), 100);
        assert_eq!(total, 1, "{:?}", names_of(&s, &slots));
        assert_eq!(names_of(&s, &slots), vec!["xxx"]);

        // 多段从末尾往前连续：aa\DeepSeek\xxx 命中；other\DeepSeek\zzz 不会误配
        // aa\aa\DeepSeek\zzz（那个目录不存在）。
        let (slots, _, _) = query_volume(&s, &chain_spec(&["aa", "DeepSeek", "xxx"], false), 100);
        assert_eq!(names_of(&s, &slots), vec!["xxx"]);
        let (slots, _, _) = query_volume(
            &s,
            &chain_spec(&["aa", "aa", "DeepSeek", "zzz"], false),
            100,
        );
        assert!(slots.is_empty(), "{:?}", names_of(&s, &slots));

        // 段内按子串匹配（Deep 命中 DeepSeek），与名字搜索一致。
        let (slots, _, _) = query_volume(&s, &chain_spec(&["Deep", "xxx"], false), 100);
        assert_eq!(names_of(&s, &slots), vec!["xxx"]);
        // 最后一段绑定名字：report.txt 不在 xxx 里，不会被 DeepSeek\xxx 带出。
        assert!(!names_of(&s, &slots).contains(&"report.txt".to_string()));

        // 大小写不敏感。
        let (slots, _, _) = query_volume(&s, &chain_spec(&["deepseek", "XXX"], false), 100);
        assert_eq!(names_of(&s, &slots), vec!["xxx"]);
    }

    #[test]
    fn path_chain_trailing_matches_descendants() {
        let s = path_chain_store();
        // DeepSeek\：任意 DeepSeek 目录下所有后代（两个卷内 DeepSeek 都算）。
        let (slots, total, _) = query_volume(&s, &chain_spec(&["DeepSeek"], true), 100);
        assert_eq!(total, 4, "{:?}", names_of(&s, &slots));
        assert_eq!(
            names_of(&s, &slots),
            vec!["report.txt", "xxx", "yyy.txt", "zzz"]
        );
        // 带盘符同款语义不存在时，用多段限定到 aa\DeepSeek\。
        let (slots, total, _) = query_volume(&s, &chain_spec(&["aa", "DeepSeek"], true), 100);
        assert_eq!(total, 3, "{:?}", names_of(&s, &slots));
        assert_eq!(names_of(&s, &slots), vec!["report.txt", "xxx", "yyy.txt"]);
    }

    #[test]
    fn path_chain_combines_with_prefix_constraint() {
        let s = path_chain_store();
        // 目录上下文（other）里的 DeepSeek\zzz：前缀与分段链同时生效。
        let mut spec = chain_spec(&["DeepSeek", "zzz"], false);
        spec.path_prefix = Some(r"e:\other".into());
        let (slots, total, _) = query_volume(&s, &spec, 100);
        assert_eq!(total, 1, "{:?}", names_of(&s, &slots));
        assert_eq!(names_of(&s, &slots), vec!["zzz"]);

        // 前缀指向 aa 时，other\DeepSeek\zzz 必须被排除。
        let mut spec = chain_spec(&["DeepSeek", "zzz"], false);
        spec.path_prefix = Some(r"e:\aa".into());
        let (slots, total, _) = query_volume(&s, &spec, 100);
        assert_eq!(total, 0, "{:?}", names_of(&s, &slots));
    }

    #[test]
    fn query_is_cancelled_when_newer_generation_arrives() {
        let mut s = VolumeStore::new(Default::default(), 'C');
        for i in 0..50_000u32 {
            s.insert(1000 + i as u64, 0, 0, format!("file{i}.txt").as_bytes());
        }
        s.compact();
        let spec = QuerySpec {
            tokens: vec!["file".into()],
            ..Default::default()
        };
        let my = CANCEL_GEN.load(Ordering::Relaxed);
        let (_, total, _, aborted) = query_volume_cancel(&s, &spec, 2000, Some(my));
        assert_eq!(total, 50_000);
        assert!(!aborted, "代次未变时不应中止");
        CANCEL_GEN.store(my + 1, Ordering::Relaxed);
        let (_, _, _, aborted) = query_volume_cancel(&s, &spec, 2000, Some(my));
        assert!(aborted, "代次变化后旧查询应提前中止");
        CANCEL_GEN.store(my, Ordering::Relaxed);
    }

    /// B 步:mtime 过滤(内置时间筛选)必须排除 mtime=0(未知)与早于下界的条目。
    #[test]
    fn mtime_filter_excludes_unknown_and_old_entries() {
        let mut s = VolumeStore::new(Default::default(), 'C');
        s.insert_meta(5, 0, 0x10, b"root", 1_600_000_000, u64::MAX);
        s.insert_meta(10, 5, 0, b"new.txt", 1_800_000_000, u64::MAX);
        s.insert_meta(11, 5, 0, b"old.txt", 1_500_000_000, u64::MAX);
        s.insert(12, 5, 0, b"unknown.txt");
        s.compact();
        let spec = QuerySpec {
            tokens: vec!["txt".into()],
            mtime_after: Some(1_700_000_000),
            ..Default::default()
        };
        let (slots, total, _) = query_volume(&s, &spec, 100);
        assert_eq!(total, 1);
        assert_eq!(s.name(slots[0]), b"new.txt");
    }

    #[test]
    fn size_filter_excludes_dirs_unknown_and_out_of_range() {
        let mut s = VolumeStore::new(Default::default(), 'C');
        s.insert_meta(5, 0, 0x10, b"root", 0, 0); // 目录：不参与大小
        s.insert_meta(10, 5, 0, b"small.txt", 0, 500);
        s.insert_meta(11, 5, 0, b"mid.txt", 0, 10 * 1024 * 1024);
        s.insert_meta(12, 5, 0, b"big.txt", 0, 3 * 1024 * 1024 * 1024);
        s.insert_meta(13, 5, 0, b"unknown.txt", 0, u64::MAX);
        s.compact();

        let spec = QuerySpec {
            size_after: Some(1024),
            ..Default::default()
        };
        let (slots, total, _) = query_volume(&s, &spec, 100);
        let mut names: Vec<&[u8]> = slots.iter().map(|&x| s.name(x)).collect();
        names.sort_unstable();
        assert_eq!(total, 2, "目录/未知大小/过小文件都应被排除");
        assert_eq!(names, vec![&b"big.txt"[..], &b"mid.txt"[..]]);

        let range = QuerySpec {
            size_after: Some(10 * 1024 * 1024),
            size_before: Some(2 * 1024 * 1024 * 1024),
            ..Default::default()
        };
        let (slots, total, _) = query_volume(&s, &range, 100);
        assert_eq!(total, 1, "只有 mid 落在区间内（边界含等号）");
        assert_eq!(s.name(slots[0]), b"mid.txt");
    }

    /// E:单字符走字母位图快路径,结果应与名字里含该字母的条目一致。
    #[test]
    fn single_char_uses_char_mask_and_matches_names() {        let s = setup();
        let spec = QuerySpec { tokens: vec!["a".into()], ..Default::default() };
        let (slots, total, _) = query_volume(&s, &spec, 100);
        assert_eq!(total, 3, "alpha/Program Files/xabcxabc 含 a");
        let names: Vec<&[u8]> = slots.iter().map(|&x| s.name(x)).collect();
        assert!(names.contains(&&b"alpha.txt"[..]));
        assert!(names.contains(&&b"Program Files"[..]));
    }

    fn setup() -> VolumeStore {
        let mut s = VolumeStore::new(Default::default(), 'C');
        s.insert(5, 0, 0x10, b"root");
        s.insert(10, 5, 0, b"alpha.txt");
        s.insert(11, 5, 0x10, b"Program Files");
        s.insert(12, 5, 0, b"myprog.exe");
        s.insert(13, 5, 0, b"xabcxabc.txt");
        s.compact();
        s
    }

    #[test]
    fn substring_and_dedupe_within_name() {
        let s = setup();
        let spec = QuerySpec {
            tokens: vec!["prog".into()],
            kind: KindFilter::All,
            ..Default::default()
        };
        let (slots, total, _) = query_volume(&s, &spec, 2000);
        assert_eq!(total, 2); // Program Files + myprog.exe
        let names: Vec<&[u8]> = slots.iter().map(|&x| s.name(x)).collect();
        assert!(names.contains(&&b"Program Files"[..]));
        assert!(names.contains(&&b"myprog.exe"[..]));
        // "abc" 在 xabcxabc.txt 命中两次,只算一条
        let spec = QuerySpec {
            tokens: vec!["abc".into()],
            kind: KindFilter::All,
            ..Default::default()
        };
        let (_, total, _) = query_volume(&s, &spec, 2000);
        assert_eq!(total, 1);
    }

    #[test]
    fn files_scope_excludes_exe_and_dirs() {
        let s = setup();
        let spec = QuerySpec {
            tokens: vec!["prog".into()],
            kind: KindFilter::Files,
            ..Default::default()
        };
        let (_, total, _) = query_volume(&s, &spec, 2000);
        assert_eq!(total, 0); // Program Files=目录,myprog.exe=exe
        let spec = QuerySpec {
            tokens: vec!["prog".into()],
            kind: KindFilter::Programs,
            ..Default::default()
        };
        let (slots, total, _) = query_volume(&s, &spec, 2000);
        assert_eq!(total, 1);
        assert_eq!(s.name(slots[0]), b"myprog.exe");
    }

    #[test]
    fn ext_and_group_filters() {
        let s = setup();
        let spec = QuerySpec {
            tokens: vec!["a".into()],
            kind: KindFilter::All,
            ext: Some("txt".into()),
            ..Default::default()
        };
        let (_, total, _) = query_volume(&s, &spec, 2000);
        assert_eq!(total, 2); // alpha.txt + xabcxabc.txt
        let spec = QuerySpec {
            tokens: vec!["prog".into()],
            kind: KindFilter::All,
            group_exts: vec!["exe".into()],
            ..Default::default()
        };
        let (_, total, _) = query_volume(&s, &spec, 2000);
        assert_eq!(total, 1); // myprog.exe
    }

    #[test]
    fn cap_bounds_collection_but_total_exact() {
        let mut s = VolumeStore::new(Default::default(), 'C');
        for i in 0..100u64 {
            s.insert(1000 + i, 5, 0, format!("dup{i:03}.txt").as_bytes());
        }
        s.insert(5, 0, 0x10, b"root");
        s.compact();
        let spec = QuerySpec {
            tokens: vec!["dup".into()],
            kind: KindFilter::Files,
            ..Default::default()
        };
        let (slots, total, capped) = query_volume(&s, &spec, 10);
        assert_eq!(total, 100);
        assert_eq!(slots.len(), 10);
        assert!(capped);
    }

    /// 任务 4 基准(手动运行,操作全局 CANCEL_GEN,不进默认套件):
    /// 取消检查的候选粒度 —— 在遍历中途改写代次,统计「换代次后还多走了
    /// 多少候选才中止」,再乘单候选实测成本折算中止延迟。
    /// `cargo test -p lstr-index --release --lib bench_cancel_granularity -- --ignored --nocapture`
    #[test]
    #[ignore = "manual bench: mutates global CANCEL_GEN"]
    fn bench_cancel_granularity() {
        let mut s = VolumeStore::new(Default::default(), 'C');
        s.insert(5, 0, 0x10, b"root");
        for i in 0..1_000_000u64 {
            s.insert(1000 + i, 5, 0, format!("alpha_{i:06}.txt").as_bytes());
        }
        s.compact();
        // 慢路径(memmem + 逐命中二分)便于在中途改写代次。
        let spec = QuerySpec {
            tokens: vec!["_".into()],
            kind: KindFilter::All,
            ext: Some("txt".into()),
            ..Default::default()
        };
        let tokens = query_tokens(&spec);
        let my = CANCEL_GEN.load(Ordering::Relaxed);
        let mut emitted = 0usize;
        let mut cancel_at = 0usize;
        let t0 = std::time::Instant::now();
        let aborted = walk_candidates(&s, &spec, &tokens, None, None, Some(my), &mut |_slot| {
            emitted += 1;
            if emitted == 100_000 {
                CANCEL_GEN.store(my + 1, Ordering::Relaxed);
                cancel_at = emitted;
            }
            true
        });
        let elapsed = t0.elapsed().as_secs_f64() * 1000.0;
        CANCEL_GEN.store(my, Ordering::Relaxed);
        assert!(aborted, "换代次后遍历必须中止");
        let overshoot = emitted - cancel_at;
        println!(
            "[cancel] memmem: emitted={emitted} 换代次后多走 {overshoot} 候选才中止 (stride={CANCEL_CHECK_EVERY}), 耗时 {elapsed:.3}ms"
        );
        assert!(
            overshoot <= 2 * CANCEL_CHECK_EVERY,
            "中止粒度应 ≤{CANCEL_CHECK_EVERY} 候选(加一次检查相位), got {overshoot}"
        );

        // 单字符位图路径(生产 "a" 的主路径):满库扫描总耗时 → 每 1024 候选
        // 的上界延迟。无目录 + Folders 过滤 → 扫完 1M 条不做任何收集。
        let spec_bitmap = QuerySpec {
            tokens: vec!["a".into()],
            kind: KindFilter::Folders,
            ext: Some("txt".into()),
            ..Default::default()
        };
        let t1 = std::time::Instant::now();
        let (_, total, _, _) = query_volume_cancel(&s, &spec_bitmap, 2000, None);
        let bitmap_ms = t1.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(total, 0);
        let per_candidate_ns = bitmap_ms * 1_000_000.0 / 1_000_000.0;
        let bound_us = per_candidate_ns * CANCEL_CHECK_EVERY as f64 / 1000.0;
        println!(
            "[cancel] bitmap+ext: 1M 全扫 {bitmap_ms:.3}ms → 每候选 {per_candidate_ns:.2}ns, 取消上界 {bound_us:.3}µs"
        );
        // 生产形态(无 ext/组过滤):passes_filters 快路径只读 attrs。
        let spec_plain = QuerySpec {
            tokens: vec!["a".into()],
            kind: KindFilter::Folders,
            ..Default::default()
        };
        let t2 = std::time::Instant::now();
        let (_, total2, _, _) = query_volume_cancel(&s, &spec_plain, 2000, None);
        let plain_ms = t2.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(total2, 0);
        let plain_ns = plain_ms * 1_000_000.0 / 1_000_000.0;
        let plain_bound_us = plain_ns * CANCEL_CHECK_EVERY as f64 / 1000.0;
        println!(
            "[cancel] bitmap(attrs-only): 1M 全扫 {plain_ms:.3}ms → 每候选 {plain_ns:.2}ns, 取消上界 {plain_bound_us:.3}µs"
        );
        assert!(plain_ms * 3.0 < 10.0, "2.9M 条生产规模全扫应在 10ms 内(旧实现 folders 路 39ms)");
    }

    /// 任务 1 对拍:一次 split 遍历 ≡ 两次独立查询(folders 路 + All 路)。
    /// 逐项比较 slots 顺序与两个 total;覆盖单字符位图、2/3 字符链、≥4 字符
    /// trigram 验证、ext/组过滤、空 token 过滤器、scan 模式与 overlay。
    fn assert_split_matches_two_pass(s: &VolumeStore, spec: &QuerySpec, cap: usize) {
        let mut spec_all = spec.clone();
        spec_all.kind = KindFilter::All;
        let mut spec_folders = spec.clone();
        spec_folders.kind = KindFilter::Folders;
        let all = query_volume_cancel(s, &spec_all, cap, None);
        let folders = query_volume_cancel(s, &spec_folders, cap, None);
        let (all_slots, folder_slots, total_all, total_folders, aborted) =
            query_volume_split_cancel(s, &spec_all, cap, cap, None);
        assert!(!aborted);
        assert_eq!(all_slots, all.0, "All 前缀顺序不一致: {:?}", spec.tokens);
        assert_eq!(total_all, all.1, "total_all 不一致: {:?}", spec.tokens);
        assert_eq!(
            folder_slots, folders.0,
            "Folders 前缀顺序不一致: {:?}",
            spec.tokens
        );
        assert_eq!(
            total_folders, folders.1,
            "total_folders 不一致: {:?}",
            spec.tokens
        );
    }

    #[test]
    fn split_query_matches_two_pass_on_mixed_store() {
        let mut s = VolumeStore::new(Default::default(), 'C');
        s.insert(5, 0, 0x10, b"root");
        for i in 0..20_000u64 {
            let slot_kind = i % 10;
            let name = match slot_kind {
                0 => format!("ai_dir_{i:05}"),
                1 => format!("ai_file_{i:05}.txt"),
                2 => format!("ai_tool_{i:05}.exe"),
                3 => format!("tail_{i:05}.log"),
                4 => format!("main_{i:05}.rs"),
                5 => format!("报告_{i:05}.docx"),
                6 => format!("alpha_{i:05}.tmp"),
                _ => format!("zip_{i:05}.dat"),
            };
            let attrs = if slot_kind == 0 { 0x10 } else { 0 };
            s.insert_meta(
                1000 + i,
                5,
                attrs,
                name.as_bytes(),
                1_600_000_000 + (i % 1000) as i64,
                u64::MAX,
            );
        }
        s.compact();
        for spec in [
            QuerySpec { tokens: vec!["ai".into()], ..Default::default() },
            QuerySpec { tokens: vec!["a".into()], ..Default::default() },
            QuerySpec { tokens: vec!["alph".into()], ..Default::default() },
            QuerySpec {
                tokens: vec!["ai".into()],
                ext: Some("txt".into()),
                ..Default::default()
            },
            QuerySpec {
                tokens: vec!["tool".into()],
                group_exts: vec!["exe".into()],
                ..Default::default()
            },
            QuerySpec {
                tokens: Vec::new(),
                ext: Some("rs".into()),
                ..Default::default()
            },
            QuerySpec {
                tokens: vec!["ai".into()],
                mtime_after: Some(1_600_000_500),
                ..Default::default()
            },
        ] {
            assert_split_matches_two_pass(&s, &spec, 1000);
        }

        // overlay:改名 + 追加后,快路径与双路仍逐项一致。
        s.insert(1001, 5, 0, b"ai_renamed.txt");
        s.insert(21_000, 5, 0, b"ai_appended.txt");
        for spec in [
            QuerySpec { tokens: vec!["ai".into()], ..Default::default() },
            QuerySpec { tokens: vec!["appended".into()], ..Default::default() },
        ] {
            assert_split_matches_two_pass(&s, &spec, 1000);
        }
        // scan 模式(memmem 兜底路径)也一致。
        let mut scan = VolumeStore::new(Default::default(), 'C');
        scan.insert(5, 0, 0x10, b"root");
        scan.insert(10, 5, 0x10, b"ai_dir");
        scan.insert(11, 5, 0, b"ai_file.txt");
        scan.insert(12, 5, 0, b"ai_tool.exe");
        assert!(scan.is_scanning());
        for spec in [
            QuerySpec { tokens: vec!["ai".into()], ..Default::default() },
            QuerySpec { tokens: vec!["a".into()], ..Default::default() },
        ] {
            assert_split_matches_two_pass(&scan, &spec, 1000);
        }
    }

    /// total 边界:All 计数先撞 TOTAL_CAP(100k)而 Folders 未撞时,split 必须
    /// 继续扫到 Folders 也到顶/库尾,两个 total 与旧双路各自的截断口径一致。
    #[test]
    fn split_total_cap_matches_two_pass() {
        let mut s = VolumeStore::new(Default::default(), 'C');
        s.insert(5, 0, 0x10, b"root");
        for i in 0..120_000u64 {
            let attrs = if i % 2 == 0 { 0x10 } else { 0 };
            s.insert(1000 + i, 5, attrs, format!("a_{i:06}.txt").as_bytes());
        }
        s.compact();
        let spec = QuerySpec { tokens: vec!["a".into()], ..Default::default() };
        assert_split_matches_two_pass(&s, &spec, 1000);
        let (all_slots, folder_slots, total_all, total_folders, aborted) =
            query_volume_split_cancel(&s, &spec, 1000, 1000, None);
        assert!(!aborted);
        assert_eq!(total_all, TOTAL_CAP, "All 计数应在 100k 截断");
        assert_eq!(total_folders, 60_000, "Folders 计数未撞上限,应精确");
        assert_eq!(all_slots.len(), 1000);
        assert_eq!(folder_slots.len(), 1000);
    }

    #[test]
    fn filter_only_counts_are_bounded() {
        // B5:空 token(纯过滤)查询不能为了精确 total 扫全卷。构造 4 万条
        // 目录+文件,split/single 都应在 FILTER_ONLY_TOTAL_CAP 处停。
        let mut s = VolumeStore::new(Default::default(), 'C');
        s.insert(5, 0, 0x10, b"root");
        for i in 0..40_000u64 {
            let attrs = if i % 2 == 0 { 0x10 } else { 0 };
            s.insert(1000 + i, 5, attrs, format!("b_{i:06}.bin").as_bytes());
        }
        s.compact();
        let spec = QuerySpec::default(); // 空 tokens = 纯过滤
        let (_all, _folders, total_all, total_folders, aborted) =
            query_volume_split_cancel(&s, &spec, 100, 100, None);
        assert!(!aborted);
        assert_eq!(
            total_all, FILTER_ONLY_TOTAL_CAP,
            "纯过滤 split 应在预算处停扫,total 作为下界"
        );
        assert!(total_folders <= total_all);
        let (slots, total, _capped, aborted) = query_volume_cancel(&s, &spec, 100, None);
        assert!(!aborted);
        assert_eq!(total, FILTER_ONLY_TOTAL_CAP, "纯过滤 single 也应停在预算处");
        assert_eq!(slots.len(), 100);
    }

    #[test]
    fn scan_mode_query_works_before_compact() {
        let mut s = VolumeStore::new(Default::default(), 'C');
        s.insert(5, 0, 0x10, b"root");
        s.insert(10, 5, 0, b"alpha.txt");
        s.insert(11, 5, 0, b"alphabet.dat");
        assert!(s.is_scanning());
        let spec = QuerySpec {
            tokens: vec!["alpha".into()],
            kind: KindFilter::All,
            ..Default::default()
        };
        let (slots, total, _) = query_volume(&s, &spec, 2000);
        assert_eq!(total, 2);
        assert_eq!(slots.len(), 2);
    }

    /// MAJOR 回归:空卷首扫完成(VolumeReady → compact)后新建文件,
    /// ≥2 字符纯 ASCII 查询曾被空倒排快路径静默归零。空 store 不得建倒排,
    /// 插入不得被 scan 模式挡住,查询必须回落 memmem 并命中。
    #[test]
    fn empty_compact_then_insert_is_searchable() {
        let mut s = VolumeStore::new(Default::default(), 'C');
        assert!(s.is_scanning());
        s.compact();
        assert!(!s.is_scanning(), "空卷 compact 后必须退出 scan 模式");
        assert!(s.is_empty());
        assert!(s.ngram_candidates(b"alpha").is_none(), "空 store 无倒排");

        assert!(s.insert(10, 5, 0, b"alpha.txt"));
        // 每个查询形态都必须命中,且与强制 memmem 路径逐字段一致
        for q in ["alpha", "al", "ph", "alpha.txt"] {
            let spec = QuerySpec { tokens: vec![q.into()], ..Default::default() };
            let (slots, total, _) = query_volume(&s, &spec, 100);
            assert_eq!(total, 1, "查询 {q:?} 应命中空卷 compact 后新建的文件");
            assert_eq!(s.name(slots[0]), b"alpha.txt");
            assert_equivalent(&s, &spec);
        }
        // 继续新建:倒排要到下次 compact 才建,查询仍走 memmem 兜底
        assert!(s.insert(11, 5, 0, b"beta.log"));
        let spec = QuerySpec { tokens: vec!["be".into()], ..Default::default() };
        let (_, total, _) = query_volume(&s, &spec, 100);
        assert_eq!(total, 1);
        assert_equivalent(&s, &spec);
    }

    #[test]
    fn empty_tokens_list_by_filters() {
        // H5:只有过滤器(无文字 token)时按过滤条件全量列举,而不是返回空。
        let mut s = VolumeStore::new(Default::default(), 'C');
        s.insert(5, 0, 0x10, b"root");
        s.insert(10, 5, 0, b"a.rs");
        s.insert(11, 5, 0, b"b.txt");
        s.insert(12, 5, 0x10, b"dir.rs");
        let spec = QuerySpec {
            tokens: Vec::new(),
            kind: KindFilter::Files,
            ext: Some("rs".into()),
            ..Default::default()
        };
        let (slots, total, _) = query_volume(&s, &spec, 100);
        assert_eq!(total, 1, "目录与 .txt 都不应计入");
        assert_eq!(slots.len(), 1);
        assert_eq!(s.name(slots[0]), b"a.rs");
    }

    #[test]
    fn case_folding_matches_uppercase_query() {
        let s = setup();
        let spec = QuerySpec {
            tokens: vec!["PROGRAM".into()],
            kind: KindFilter::All,
            ..Default::default()
        };
        let (_, total, _) = query_volume(&s, &spec, 2000);
        // "program files" 前缀命中;"myprog.exe" 只含 "prog" 不含 "program"
        assert_eq!(total, 1);
    }

    // ---------- n-gram 快路径等价性:与 memmem 路径同序同分 ----------

    /// 区分大小写：只命中原始字节一致的名字；默认模式保持大小写不敏感。
    #[test]
    fn case_sensitive_matches_raw_names_only() {
        let mut s = VolumeStore::new(Default::default(), 'C');
        for (i, name) in ["README", "readme", "ReadMe", "report.txt", "ÄI", "äi"]
            .iter()
            .enumerate()
        {
            s.insert(100 + i as u64, 0, 0, name.as_bytes());
        }
        s.compact();
        let run = |token: &str, case_sensitive: bool| {
            let spec = QuerySpec {
                tokens: vec![token.to_string()],
                case_sensitive,
                ..Default::default()
            };
            let (_, total, _) = query_volume(&s, &spec, 100);
            total
        };
        assert_eq!(run("README", true), 1);
        assert_eq!(run("readme", true), 1);
        assert_eq!(run("ReadMe", true), 1);
        assert_eq!(run("read", true), 1, "只应命中全小写的 readme");
        assert_eq!(run("read", false), 3, "默认大小写不敏感仍命中三例");
        // 区分大小写:非 ASCII 仍按原始字节精确(MatchMatcher 用 store.name)。
        assert_eq!(run("ÄI", true), 1);
        assert_eq!(run("äi", true), 1);
        // A-6:大小写不敏感时 Unicode 简单折叠使 Ä/ä 互配(旧 fold_ascii 不互配)。
        assert_eq!(run("äi", false), 2, "ÄI 与 äi 都应命中");
        assert_eq!(run("ÄI", false), 2);
    }

    fn whole_word_store() -> VolumeStore {
        let mut s = VolumeStore::new(Default::default(), 'C');
        for (i, name) in [
            "foo",
            "foo.txt",
            "my-foo-bar",
            "foobar",
            "xfoo",
            "(foo)",
            "foo_bar",
            "FOO",
        ]
        .iter()
        .enumerate()
        {
            s.insert(100 + i as u64, 0, 0, name.as_bytes());
        }
        s.compact();
        s
    }

    #[test]
    fn whole_word_requires_boundaries() {
        let s = whole_word_store();
        let spec = QuerySpec {
            tokens: vec!["foo".into()],
            whole_word: true,
            ..Default::default()
        };
        let (slots, total, _) = query_volume(&s, &spec, 100);
        assert_eq!(total, 6);
        let names: Vec<&[u8]> = slots.iter().map(|&x| s.name(x)).collect();
        for expected in [&b"foo"[..], b"foo.txt", b"my-foo-bar", b"(foo)", b"foo_bar", b"FOO"] {
            assert!(names.contains(&expected), "缺少 {:?}: {names:?}", expected);
        }
        for unexpected in [&b"foobar"[..], b"xfoo"] {
            assert!(!names.contains(&unexpected), "不应命中 {:?}", unexpected);
        }

        // 区分大小写 + 全词：FOO 不再命中。
        let spec = QuerySpec {
            tokens: vec!["foo".into()],
            whole_word: true,
            case_sensitive: true,
            ..Default::default()
        };
        let (_, total, _) = query_volume(&s, &spec, 100);
        assert_eq!(total, 5);
    }

    #[test]
    fn whole_word_multi_token_is_and() {
        let mut s = VolumeStore::new(Default::default(), 'C');
        for (i, name) in ["foo-bar", "bar-foo", "foo bar baz", "foobar bar"]
            .iter()
            .enumerate()
        {
            s.insert(100 + i as u64, 0, 0, name.as_bytes());
        }
        s.compact();
        let spec = QuerySpec {
            tokens: vec!["foo".into(), "bar".into()],
            whole_word: true,
            ..Default::default()
        };
        let (slots, total, _) = query_volume(&s, &spec, 100);
        assert_eq!(total, 3, "foobar 里的 foo 不是整词，应被排除");
        let names: Vec<&[u8]> = slots.iter().map(|&x| s.name(x)).collect();
        assert!(!names.contains(&&b"foobar bar"[..]));
    }

    /// 严格匹配必须前置到 cap 之前：全盘大量非整词命中不能把整词命中挤掉。
    #[test]
    fn whole_word_filters_before_candidate_cap() {
        let mut s = VolumeStore::new(Default::default(), 'C');
        for i in 0..3000u64 {
            s.insert(1000 + i, 0, 0, format!("foobar{i}.txt").as_bytes());
        }
        s.insert(9000, 0, 0, b"foo");
        s.compact();
        let spec = QuerySpec {
            tokens: vec!["foo".into()],
            whole_word: true,
            ..Default::default()
        };
        let (slots, total, _) = query_volume(&s, &spec, 10);
        assert_eq!(total, 1);
        assert_eq!(s.name(slots[0]), b"foo");
    }

    #[test]
    fn wildcard_prefix_and_suffix_filter_names() {
        let mut s = VolumeStore::new(Default::default(), 'C');
        for (i, name) in ["abc.txt", "abc.html", "xabc.txt", "abcd.txt", "report.abc"]
            .iter()
            .enumerate()
        {
            s.insert(100 + i as u64, 0, 0, name.as_bytes());
        }
        s.compact();

        let spec = QuerySpec {
            prefix: Some("abc.".into()),
            ..Default::default()
        };
        let (slots, total, _) = query_volume(&s, &spec, 100);
        let mut names: Vec<&[u8]> = slots.iter().map(|&slot| s.name(slot)).collect();
        names.sort();
        assert_eq!(total, 2);
        assert_eq!(names, vec![&b"abc.html"[..], b"abc.txt"]);

        let spec = QuerySpec {
            suffix: Some(".txt".into()),
            ..Default::default()
        };
        let (slots, total, _) = query_volume(&s, &spec, 100);
        assert_eq!(total, 3);
        let mut names: Vec<&[u8]> = slots.iter().map(|&slot| s.name(slot)).collect();
        names.sort();
        assert_eq!(
            names,
            vec![&b"abc.txt"[..], b"abcd.txt", b"xabc.txt"]
        );
    }

    /// 通配过滤必须先于 cap：目录/全盘里大量非前缀命中不能把前缀命中挤掉。
    #[test]
    fn wildcard_prefix_filters_before_candidate_cap() {
        let mut s = VolumeStore::new(Default::default(), 'C');
        for i in 0..2000u64 {
            s.insert(1000 + i, 0, 0, format!("zzz{i}.txt").as_bytes());
        }
        s.insert(9000, 0, 0, b"abc-target.txt");
        s.compact();
        let spec = QuerySpec {
            prefix: Some("abc".into()),
            ..Default::default()
        };
        let (slots, total, _) = query_volume(&s, &spec, 10);
        assert_eq!(total, 1);
        assert_eq!(s.name(slots[0]), b"abc-target.txt");
    }

    #[test]
    fn case_sensitive_works_in_scan_mode() {
        // 未 compact：ngram 不可用，走 memmem 全扫。
        let mut s = VolumeStore::new(Default::default(), 'C');
        s.insert(10, 0, 0, b"README");
        s.insert(11, 0, 0, b"readme");
        assert!(s.is_scanning());
        let spec = QuerySpec {
            tokens: vec!["README".into()],
            case_sensitive: true,
            ..Default::default()
        };
        let (_, total, _) = query_volume(&s, &spec, 100);
        assert_eq!(total, 1);
    }

    fn regex_oracle(store: &VolumeStore, pattern: &str, case_sensitive: bool) -> Vec<String> {
        let regex = RegexBuilder::new(pattern)
            .case_insensitive(!case_sensitive)
            .build()
            .expect("测试正则应合法");
        (0..store.name_off.len() as u32)
            .filter(|&slot| store.attrs[slot as usize] & ATTR_INTERNAL == 0)
            .filter(|&slot| regex.is_match(store.name(slot)))
            .map(|slot| String::from_utf8_lossy(store.name(slot)).into_owned())
            .collect()
    }

    fn regex_query(store: &VolumeStore, pattern: &str, case_sensitive: bool) -> Vec<String> {
        let spec = QuerySpec {
            regex: Some(pattern.to_string()),
            case_sensitive,
            ..Default::default()
        };
        let (slots, _, _) = query_volume(store, &spec, 2000);
        slots
            .into_iter()
            .map(|slot| String::from_utf8_lossy(store.name(slot)).into_owned())
            .collect()
    }

    #[test]
    fn regex_matches_and_respects_case_flag() {
        let mut s = VolumeStore::new(Default::default(), 'C');
        for (i, name) in ["README.md", "readme.txt", "api-ms-win-core-file.dll", "foo.log"]
            .iter()
            .enumerate()
        {
            s.insert(100 + i as u64, 0, 0, name.as_bytes());
        }
        s.compact();

        let mut got = regex_query(&s, r"^readme", false);
        got.sort();
        assert_eq!(got, vec!["README.md", "readme.txt"]);
        let got = regex_query(&s, r"^readme", true);
        assert_eq!(got, vec!["readme.txt"]);

        let got = regex_query(&s, r"^api-.*\.dll$", true);
        assert_eq!(got, vec!["api-ms-win-core-file.dll"]);
        let mut expected = regex_oracle(&s, r"^api-.*\.dll$", true);
        expected.sort();
        let mut got = got;
        got.sort();
        assert_eq!(got, expected, "字面量预筛结果必须与逐条正则一致");
    }

    /// 没有字面量的正则（只能全扫）也要与 oracle 一致。
    #[test]
    fn regex_without_literal_falls_back_to_full_scan() {
        let mut s = VolumeStore::new(Default::default(), 'C');
        for (i, name) in ["2024-11", "abc", "1234-56", "2024-11-11"]
            .iter()
            .enumerate()
        {
            s.insert(100 + i as u64, 0, 0, name.as_bytes());
        }
        s.compact();
        let pattern = r"^\d{4}-\d{2}$";
        let mut got = regex_query(&s, pattern, true);
        got.sort();
        let mut expected = regex_oracle(&s, pattern, true);
        expected.sort();
        assert_eq!(got, expected);
        assert!(got.contains(&"2024-11".to_string()));
    }

    #[test]
    fn regex_respects_path_prefix() {
        let mut s = VolumeStore::new(Default::default(), 'C');
        s.insert(10, 0, 0x10, b"target");
        s.insert(11, 10, 0, b"a.txt");
        s.insert(12, 0, 0x10, b"other");
        s.insert(13, 12, 0, b"b.txt");
        s.compact();
        let spec = QuerySpec {
            regex: Some(r".*\.txt$".to_string()),
            path_prefix: Some(r"d:\target".to_string()),
            ..Default::default()
        };
        let (slots, total, _) = query_volume(&s, &spec, 100);
        assert_eq!(total, 1);
        assert_eq!(s.name(slots[0]), b"a.txt");
    }

    #[test]
    fn invalid_regex_returns_empty_instead_of_panicking() {
        let mut s = VolumeStore::new(Default::default(), 'C');
        s.insert(10, 0, 0, b"anything.txt");
        s.compact();
        assert!(validate_regex("[", false).is_err());
        let spec = QuerySpec {
            regex: Some("[".to_string()),
            ..Default::default()
        };
        let (slots, total, _) = query_volume(&s, &spec, 100);
        assert_eq!(total, 0);
        assert!(slots.is_empty());
    }

    struct Lcg(u64);
    impl Lcg {
        fn new(seed: u64) -> Self {
            Self(seed | 1)
        }
        fn next(&mut self) -> u64 {
            // xorshift64*
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// 与 query_volume_cancel 相同的 gen 选择(最长 token,平局取后者)。
    fn gen_of(tokens: &[Vec<u8>]) -> (usize, Vec<u8>) {
        let i = tokens
            .iter()
            .enumerate()
            .max_by_key(|(_, t)| t.len())
            .map(|(x, _)| x)
            .unwrap_or(0);
        (i, tokens[i].clone())
    }

    /// 快路径(生产入口 query_volume_cancel)与强制 memmem 路径逐字段对拍:
    /// (slots, total, capped) 完全一致 —— 同序同分。覆盖多 cap 档位。
    fn assert_equivalent(s: &VolumeStore, spec: &QuerySpec) {
        let tokens: Vec<Vec<u8>> = spec
            .tokens
            .iter()
            .filter(|t| !t.is_empty())
            .map(|t| fold_case(t.as_bytes()))
            .collect();
        let (gi, g) = gen_of(&tokens);
        for cap in [3usize, 100, 100_000] {
            let fast = query_volume_cancel(s, spec, cap, None);
            let slow = query_volume_memmem_scan(s, spec, &tokens, gi, &g, cap, None);
            assert_eq!(
                fast, slow,
                "快路径与 memmem 不一致: tokens={:?} cap={cap}",
                spec.tokens
            );
        }
    }

    fn random_spec(rng: &mut Lcg) -> QuerySpec {
        // 混合字母表:大小写(折叠)、数字、点/下划线、空格、非 ASCII。
        // 主 token ≥2 字符(ngram 快路径适用域);空格与非 ASCII token 走
        // memmem 兜底路径,一并纳入对拍。单字符查询走位图路径(既有行为,
        // 与 memmem 的排序语义本就不同,不在本对拍范围)。
        const ALPHA: &[u8] = b"abcdefghij0123456789_. ";
        const CJK: &[&str] = &["数", "据", "报", "告"];
        let len = 2 + rng.below(6);
        let mut t = String::new();
        for _ in 0..len {
            if rng.below(8) == 0 {
                t.push_str(CJK[rng.below(CJK.len())]);
            } else {
                let c = ALPHA[rng.below(ALPHA.len())] as char;
                if rng.below(4) == 0 {
                    t.push(c.to_ascii_uppercase());
                } else {
                    t.push(c);
                }
            }
        }
        let mut spec = QuerySpec { tokens: vec![t], ..Default::default() };
        if rng.below(5) == 0 {
            // 多 token AND(第二 token 可能比主 token 短,gen 仍是主 token)
            spec.tokens.push("e".into());
        }
        spec
    }

    fn random_name(rng: &mut Lcg) -> String {
        const ALPHA: &[u8] = b"aAbBcCdDeEfFgG0123456789_.- xz";
        let len = 1 + rng.below(16);
        let mut n = String::new();
        for _ in 0..len {
            if rng.below(10) == 0 {
                n.push_str(["数据", "报告", "文件"][rng.below(3)]);
            } else {
                n.push(ALPHA[rng.below(ALPHA.len())] as char);
            }
        }
        n
    }

    /// 随机名集 + 随机查询词 + 随机维护流(新建/改名/删除/复活 + 周期 compact),
    /// 全程快路径 ≡ memmem。覆盖:scan 模式、delta overlay、改名重排、
    /// 大小写折叠、空格/非 ASCII 兜底、多 token AND。
    #[test]
    fn ngram_fast_path_matches_memmem_random_stream() {
        let mut rng = Lcg::new(0x5EED_2026_1117_0001);
        let mut s = VolumeStore::new(Default::default(), 'C');
        let mut next_frn = 100u64;
        let mut alive: Vec<u64> = Vec::new();
        let mut dead: Vec<(u64, u32, String)> = Vec::new(); // (frn, attrs, name)
        // 种子集
        for _ in 0..300 {
            let frn = next_frn;
            next_frn += 1;
            let name = random_name(&mut rng);
            let attrs = if rng.below(5) == 0 { 0x10 } else { 0 };
            s.insert(frn, 0, attrs, name.as_bytes());
            alive.push(frn);
        }
        let query_batch = |s: &VolumeStore, rng: &mut Lcg| {
            for _ in 0..6 {
                let spec = random_spec(rng);
                assert_equivalent(s, &spec);
            }
        };
        // scan 模式(无索引,快路径应自动兜底)就对拍
        query_batch(&s, &mut rng);
        s.compact();
        assert!(!s.is_scanning(), "非空 compact 后必须退出 scan 模式");

        // 空 store 回归:删光存活项再 compact(空卷 compact 路径),之后
        // 插入的条目必须仍可查;倒排保持 None → 查询走 memmem 兜底。
        for frn in alive.drain(..) {
            s.remove(frn);
        }
        s.compact();
        assert!(!s.is_scanning(), "空卷 compact 后也必须退出 scan 模式");
        assert!(s.is_empty());
        for _ in 0..64 {
            let frn = next_frn;
            next_frn += 1;
            let name = random_name(&mut rng);
            let attrs = if rng.below(5) == 0 { 0x10 } else { 0 };
            s.insert(frn, 0, attrs, name.as_bytes());
            alive.push(frn);
        }
        assert!(s.reindex_slots().is_empty(), "无倒排时插入不进 overlay");
        query_batch(&s, &mut rng);

        for step in 0..1200 {
            match rng.below(100) {
                0..=44 => {
                    let frn = next_frn;
                    next_frn += 1;
                    let name = random_name(&mut rng);
                    let attrs = if rng.below(5) == 0 { 0x10 } else { 0 };
                    s.insert(frn, 0, attrs, name.as_bytes());
                    alive.push(frn);
                }
                45..=64 if !alive.is_empty() => {
                    // 就地改名(旧名 posting 失效/新名进 overlay)
                    let pos = rng.below(alive.len());
                    let frn = alive[pos];
                    let name = random_name(&mut rng);
                    s.insert(frn, 0, 0, name.as_bytes());
                }
                65..=79 if !alive.is_empty() => {
                    let pos = rng.below(alive.len());
                    let frn = alive.swap_remove(pos);
                    s.remove(frn);
                    dead.push((frn, 0, String::new()));
                }
                _ if !dead.is_empty() => {
                    // 复活(同 FRN 任意新名;名字可能变化 → 改名分支)
                    let pos = rng.below(dead.len());
                    let (frn, _, _) = dead.swap_remove(pos);
                    let name = random_name(&mut rng);
                    s.insert(frn, 0, 0, name.as_bytes());
                    alive.push(frn);
                }
                _ => {}
            }
            if step % 173 == 0 {
                s.compact();
            }
            if step % 37 == 0 {
                query_batch(&s, &mut rng);
            }
        }
        s.compact();
        query_batch(&s, &mut rng);
    }

    /// 语义级维护用例:改名槽跳过 base 链、overlay 精确补位,顺序与 memmem
    /// 偏移序一致;删除/复活/compact 前后查询不变。
    #[test]
    fn ngram_overlay_rename_remove_revive_semantics() {
        let mut s = VolumeStore::new(Default::default(), 'C');
        s.insert(5, 0, 0x10, b"root");
        s.insert(10, 5, 0, b"alphabet.txt");
        s.insert(11, 5, 0, b"beta");
        s.insert(12, 5, 0, b"gamma");
        s.compact();
        let spec = |t: &str| QuerySpec { tokens: vec![t.into()], ..Default::default() };

        // 改名 alphabet.txt → zzz.txt:slot 10 的旧名 posting 全部失效
        s.insert(10, 5, 0, b"zzz.txt");
        // "be":base 链(旧 alphabet.txt/beta)− 改名槽 = [beta]
        let (slots, total, _) = query_volume(&s, &spec("be"), 100);
        assert_eq!((slots, total), (vec![s.find_slot(11).unwrap()], 1));
        // "zzz":不在 base 链(构建时无此名),只能由 overlay 命中
        let (slots, total, _) = query_volume(&s, &spec("zzz"), 100);
        assert_eq!(total, 1);
        assert_eq!(s.name(slots[0]), b"zzz.txt");
        // 改回含 "aa" 的名字并追加一个 delta 槽,验证 overlay 按偏移排序
        s.insert(10, 5, 0, b"aaz.txt");
        s.insert(13, 5, 0, b"aabc");
        let (slots, total, _) = query_volume(&s, &spec("aa"), 100);
        assert_eq!(total, 2);
        assert_eq!(s.name(slots[0]), b"aaz.txt");
        assert_eq!(s.name(slots[1]), b"aabc");
        assert_equivalent(&s, &spec("aa"));
        assert_equivalent(&s, &spec("zzz"));
        assert_equivalent(&s, &spec("be"));

        // 删除:可见性过滤,倒排与 overlay 都不再命中
        s.remove(13);
        let (_, total, _) = query_volume(&s, &spec("aa"), 100);
        assert_eq!(total, 1);
        // 复活(名字未变):base posting 仍有效
        s.insert(13, 5, 0, b"aabc");
        let (_, total, _) = query_volume(&s, &spec("aa"), 100);
        assert_eq!(total, 2);
        // 改名 + 删除 + 复活(新名走 overlay,旧名不再命中)
        s.insert(13, 5, 0, b"aabc-renamed");
        s.remove(13);
        s.insert(13, 5, 0, b"aabc-renamed");
        assert_equivalent(&s, &spec("aabc"));
        assert_equivalent(&s, &spec("renamed"));
        s.compact();
        assert_equivalent(&s, &spec("aa"));
        assert_equivalent(&s, &spec("aabc"));
        assert_equivalent(&s, &spec("zzz"));
        assert_equivalent(&s, &spec("renamed"));
        assert_equivalent(&s, &spec("be"));
    }

    /// 死槽随快照残留(删除数 < compact 阈值时 save 不触发 compact,attrs 带
    /// DEAD 位落盘)→ 加载后倒排仍覆盖这些槽,原地复活必须可查。
    #[test]
    fn ngram_covers_dead_slots_surviving_snapshot_load() {
        let mut s = VolumeStore::new(Default::default(), 'C');
        s.insert(5, 0, 0x10, b"root");
        s.insert(10, 5, 0, b"ghost.txt");
        s.insert(11, 5, 0, b"keep.txt");
        s.compact();
        s.remove(10); // dead_count=1 < 4096:save 不 compact
        let dir = std::env::temp_dir().join(format!("lstr-ngram-dead-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let h = crate::snapshot::SnapshotHeader {
            guid: 0xDEAD_BEEF_1234,
            letter: 'C',
            complete: true,
            noise_hash: 0,
            journal_id: 1,
            next_usn: 1,
            resume_frn: 0,
            est_records: 0,
        };
        crate::snapshot::save(&dir, &h, &mut s).unwrap();
        let mut s =
            crate::snapshot::load(&dir, h.guid, 0, &Default::default()).unwrap().store;
        let spec = |t: &str| QuerySpec { tokens: vec![t.into()], ..Default::default() };
        let (_, total, _) = query_volume(&s, &spec("ghost"), 100);
        assert_eq!(total, 0, "死槽不可见");
        s.insert(10, 5, 0, b"ghost.txt"); // 原地复活
        let (slots, total, _) = query_volume(&s, &spec("ghost"), 100);
        assert_eq!(total, 1, "复活后必须可查(倒排覆盖加载时的死槽)");
        assert_eq!(s.name(slots[0]), b"ghost.txt");
        assert_equivalent(&s, &spec("ghost"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod regex_literal_tests {
    //! A-1 回归：正则字面量预筛不得产生假阴性。旧实现按字节扫最长字母数字串，
    //! 会把交替分支 / 字符类 / 转义类里的片段误当必然字面量。现改为 HIR 提取。
    use super::{query_volume, regex_literal, QuerySpec};
    use crate::store::VolumeStore;

    fn store_with(names: &[&str]) -> VolumeStore {
        let mut s = VolumeStore::new(Default::default(), 'C');
        for (i, n) in names.iter().enumerate() {
            s.insert(100 + i as u64, 0, 0, n.as_bytes());
        }
        s.compact(); // 建倒排，确保走 n-gram 预筛路径
        s
    }

    fn regex_hits(s: &VolumeStore, pattern: &str, case_sensitive: bool) -> Vec<String> {
        let spec = QuerySpec {
            regex: Some(pattern.to_string()),
            case_sensitive,
            ..Default::default()
        };
        let (slots, _, _) = query_volume(s, &spec, 2000);
        let mut names: Vec<String> = slots
            .iter()
            .map(|&slot| String::from_utf8_lossy(s.name(slot)).into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn alternation_matches_all_branches() {
        let s = store_with(&["foo.txt", "bar.txt", "baz.txt"]);
        let h = regex_hits(&s, "(foo|bar)", true);
        assert_eq!(h, vec!["bar.txt".to_string(), "foo.txt".to_string()]);
    }

    #[test]
    fn char_class_is_not_a_literal() {
        let s = store_with(&["xabc.txt", "a-zabc.txt", "zzz.txt"]);
        let h = regex_hits(&s, "[a-z]abc", true);
        assert!(h.contains(&"xabc.txt".to_string()), "xabc 应命中 [a-z]abc: {h:?}");
        assert!(!h.contains(&"zzz.txt".to_string()));
    }

    #[test]
    fn escaped_class_does_not_leak_literal() {
        let s = store_with(&["xfoo.txt", "wfoo.txt", "nope.txt"]);
        let h = regex_hits(&s, r"\wfoo", true);
        assert_eq!(h, vec!["wfoo.txt".to_string(), "xfoo.txt".to_string()]);
    }

    #[test]
    fn anchored_literal_still_prefilters_correctly() {
        let s = store_with(&["report_2024.pdf", "other.txt", "report_.pdf"]);
        let h = regex_hits(&s, r"^report_\d+\.pdf$", true);
        assert_eq!(h, vec!["report_2024.pdf".to_string()]);
    }

    #[test]
    fn case_insensitive_prefilter_matches_uppercase_name() {
        let s = store_with(&["REPORT_2024.pdf", "other.txt"]);
        let h = regex_hits(&s, r"^report_\d+\.pdf$", false);
        assert_eq!(h, vec!["REPORT_2024.pdf".to_string()]);
    }

    #[test]
    fn required_literal_extraction() {
        assert_eq!(regex_literal("(foo|bar)"), None, "交替无必然字面量");
        assert_eq!(regex_literal("[a-z]abc"), Some(b"abc".to_vec()));
        assert_eq!(regex_literal(r"\wfoo"), Some(b"foo".to_vec()));
        assert_eq!(regex_literal(r"^report_\d+\.pdf$"), Some(b"report_".to_vec()));
        assert_eq!(regex_literal("abc"), Some(b"abc".to_vec()));
        assert_eq!(regex_literal(r"a?bc"), Some(b"bc".to_vec()), "可选 a 后的 bc 仍必然");
        assert_eq!(regex_literal(r"(abc)+"), Some(b"abc".to_vec()));
        assert_eq!(regex_literal(r"abc*"), Some(b"ab".to_vec()), "c 可选重复，ab 仍必然");
        assert_eq!(regex_literal(r"a*b*c*"), None, "全部可选，无必然字面量");
    }
}

#[cfg(test)]
mod prefix_slots_tests {
    //! A-3 回归：宽查询的前缀命中不被子串候选窗挤出。
    use super::{prefix_slots, query_volume, QuerySpec};
    use crate::store::VolumeStore;

    #[test]
    fn prefix_supplement_recovers_cut_prefix_match() {
        // 3000 个低 FRN 的子串命中占满 cap=2000,高 FRN 的前缀命中 sub_target
        // 被挤出主候选窗;prefix_slots 用二分把它找回。
        let mut s = VolumeStore::new(Default::default(), 'C');
        for i in 0..3000u64 {
            s.insert(1000 + i, 0, 0, format!("a_sub_{i:04}.txt").as_bytes());
        }
        s.insert(99999, 0, 0, b"sub_target.txt");
        s.compact();
        let spec = QuerySpec { tokens: vec!["sub".into()], ..Default::default() };
        let (slots, total, capped) = query_volume(&s, &spec, 2000);
        assert!(capped, "应触发候选封顶");
        assert!(total > 2000);
        assert!(
            !slots.iter().any(|&x| s.name(x) == b"sub_target.txt"),
            "前缀命中确实被主候选窗挤出"
        );
        let pre = prefix_slots(&s, &spec, b"sub", 200);
        assert!(
            pre.iter().any(|&x| s.name(x) == b"sub_target.txt"),
            "前缀补集应找回前缀命中"
        );
    }

    #[test]
    fn prefix_slots_case_sensitive_and_cap() {
        let mut s = VolumeStore::new(Default::default(), 'C');
        s.insert(1, 0, 0, b"Foo.txt");
        s.insert(2, 0, 0, b"foo.txt");
        s.insert(3, 0, 0, b"foobar.txt");
        s.insert(4, 0, 0, b"bar.txt");
        s.compact();
        let spec = QuerySpec { tokens: vec!["foo".into()], ..Default::default() };
        let mut names: Vec<Vec<u8>> =
            prefix_slots(&s, &spec, b"foo", 200).iter().map(|&x| s.name(x).to_vec()).collect();
        names.sort();
        assert_eq!(
            names,
            vec![b"Foo.txt".to_vec(), b"foo.txt".to_vec(), b"foobar.txt".to_vec()]
        );
        // 大小写敏感:Foo 只命中 Foo.txt
        let spec_cs =
            QuerySpec { tokens: vec!["Foo".into()], case_sensitive: true, ..Default::default() };
        let cs = prefix_slots(&s, &spec_cs, b"Foo", 200);
        assert_eq!(cs.len(), 1);
        assert_eq!(s.name(cs[0]), b"Foo.txt");
        // cap 限制收集量
        assert_eq!(prefix_slots(&s, &spec, b"foo", 2).len(), 2);
    }
}
