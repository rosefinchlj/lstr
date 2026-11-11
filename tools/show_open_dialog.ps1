Add-Type -AssemblyName System.Windows.Forms
$d = New-Object System.Windows.Forms.OpenFileDialog
$d.Title = 'lstr-verify-open'
$d.InitialDirectory = 'C:\Windows'
$null = $d.ShowDialog()
