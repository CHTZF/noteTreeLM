; vcredist.nsh — Windows 安裝器前置步驟
; 若系統尚未安裝 Visual C++ 2015-2022 Redistributable (x64)，
; 透過 PowerShell 靜默下載安裝，避免 llama-server 因缺 DLL 無法啟動。

Section "-VCRedist"
  ; 僅在 64-bit 系統執行
  ${If} ${RunningX64}
    ; 檢查 VC++ 2015-2022 x64 是否已安裝（registry key）
    ReadRegDWORD $R0 HKLM "SOFTWARE\Microsoft\VisualStudio\14.0\VC\Runtimes\x64" "Installed"
    ${If} $R0 != 1
      DetailPrint "正在安裝 Visual C++ 2022 Redistributable (x64)..."
      ; 用 PowerShell 下載並靜默安裝
      nsExec::ExecToLog `powershell.exe -NonInteractive -ExecutionPolicy Bypass -Command \
        "try { \
          $f = Join-Path $env:TEMP 'vc_redist_setup.exe'; \
          (New-Object Net.WebClient).DownloadFile('https://aka.ms/vs/17/release/vc_redist.x64.exe', $f); \
          Start-Process $f '/install /quiet /norestart' -Wait; \
          Remove-Item $f -Force \
        } catch { exit 1 }"`
      Pop $R1
      ${If} $R1 != 0
        MessageBox MB_OK|MB_ICONINFORMATION \
          "Visual C++ Redistributable 安裝未完成（可能需要網路連線）。$\n\
           若 llama-server 無法啟動，請手動安裝：$\n\
           https://aka.ms/vs/17/release/vc_redist.x64.exe"
      ${EndIf}
    ${EndIf}
  ${EndIf}
SectionEnd
