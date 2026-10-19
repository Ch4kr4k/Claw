//! The `COM` tool: drives Windows applications through COM automation:
//! Word, Excel, PowerPoint, Outlook, Access, Visio, and anything else that
//! registers a ProgID (`Shell.Application`, `WScript.Shell`, WMI's
//! `WbemScripting.SWbemLocator`, `Schedule.Service`, Photoshop, AutoCAD, …).
//!
//! A COM object lives in one process, so the tool keeps one PowerShell
//! session for the whole claw session and names objects in it (`excel`,
//! `doc`, …); each call runs a little generated PowerShell there and gets
//! back a JSON line. Windows PowerShell 5.1 (`powershell.exe`) is preferred:
//! it runs single-threaded apartments and has `GetActiveObject`. Under WSL
//! the Windows `powershell.exe` is used. `CLAW_COM_POWERSHELL` names another
//! program (tests use `pwsh`, which runs the session without COM).

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};

/// Starts the line that carries a result; other output is the script's own.
const MARKER: &str = "@@CLAW-COM@@";
/// Default time one call may take (opening a large workbook is slow).
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
/// Lines a call may print besides its result.
const MAX_PRINTED: usize = 200;

/// The PowerShell side: reads one JSON request per line, runs its code in the
/// session's global scope, prints `MARKER` + a JSON result.
const HOST_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Console]::InputEncoding = New-Object System.Text.UTF8Encoding $false
[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding $false
try { Add-Type -AssemblyName Microsoft.VisualBasic } catch {}
# Missing where there is no COM (PowerShell on Linux, for tests).
$global:ClawComType = 'System.__ComObject' -as [type]
function global:Test-ClawCom($value) { return ($null -ne $global:ClawComType) -and ($value -is $global:ClawComType) }

function global:Get-ClawTypeName($value) {
  try { return [Microsoft.VisualBasic.Information]::TypeName($value) } catch { return $value.GetType().FullName }
}

function global:ConvertTo-ClawValue($value, [int]$depth = 0) {
  if ($null -eq $value) { return $null }
  if ($value -is [string] -or $value -is [bool]) { return $value }
  if ($value -is [char] -or $value -is [guid] -or $value -is [enum]) { return $value.ToString() }
  if ($value -is [datetime]) { return $value.ToString('o') }
  if ($value -is [ValueType]) {
    if ($value -is [double] -or $value -is [single]) {
      if ([double]::IsNaN($value) -or [double]::IsInfinity($value)) { return $value.ToString() }
    }
    if ($value -is [decimal] -or $value -is [double] -or $value -is [single] -or $value -is [int] -or $value -is [long] -or $value -is [short] -or $value -is [byte] -or $value -is [uint32] -or $value -is [uint64] -or $value -is [uint16] -or $value -is [sbyte]) { return $value }
    return $value.ToString()
  }
  if (Test-ClawCom $value) {
    $summary = [ordered]@{ comObject = (Get-ClawTypeName $value) }
    foreach ($property in 'Name', 'FullName', 'Count') {
      try { $item = $value.$property; if ($null -ne $item -and -not (Test-ClawCom $item)) { $summary[$property] = "$item" } } catch {}
    }
    return $summary
  }
  if ($value -is [Array] -and $value.Rank -eq 2) {
    $rows = New-Object System.Collections.ArrayList
    for ($r = $value.GetLowerBound(0); $r -le $value.GetUpperBound(0) -and $rows.Count -lt 500; $r++) {
      $row = New-Object System.Collections.ArrayList
      for ($c = $value.GetLowerBound(1); $c -le $value.GetUpperBound(1); $c++) { [void]$row.Add((ConvertTo-ClawValue $value[$r, $c] ($depth + 1))) }
      [void]$rows.Add($row.ToArray())
    }
    return ,$rows.ToArray()
  }
  if ($value -is [System.Collections.IDictionary]) {
    $map = [ordered]@{}
    foreach ($key in @($value.Keys) | Select-Object -First 200) { $map["$key"] = if ($depth -lt 3) { ConvertTo-ClawValue $value[$key] ($depth + 1) } else { "$($value[$key])" } }
    return $map
  }
  if ($value -is [System.Collections.IEnumerable]) {
    $items = New-Object System.Collections.ArrayList
    foreach ($item in $value) {
      if ($items.Count -ge 200) { break }
      [void]$items.Add($(if ($depth -lt 3) { ConvertTo-ClawValue $item ($depth + 1) } else { "$item" }))
    }
    return ,$items.ToArray()
  }
  if ($depth -lt 2) {
    $map = [ordered]@{}
    foreach ($property in $value.PSObject.Properties | Select-Object -First 40) {
      try { $map[$property.Name] = ConvertTo-ClawValue $property.Value ($depth + 1) } catch {}
    }
    if ($map.Count -gt 0) { return $map }
  }
  return "$value"
}

# A 2-D array (what Excel's Range.Value2 takes) from rows.
function global:New-Claw2D($rows) {
  $rows = @($rows)
  $width = ($rows | ForEach-Object { @($_).Count } | Measure-Object -Maximum).Maximum
  $grid = New-Object 'object[,]' $rows.Count, $width
  for ($r = 0; $r -lt $rows.Count; $r++) {
    $row = @($rows[$r])
    for ($c = 0; $c -lt $row.Count; $c++) { $grid[$r, $c] = $row[$c] }
  }
  return ,$grid
}

# The running instance of a ProgID (Windows PowerShell has it built in;
# PowerShell 7 goes through oleaut32).
function global:Get-ClawActiveObject([string]$progId) {
  $method = [System.Runtime.InteropServices.Marshal].GetMethod('GetActiveObject', [type[]]@([string]))
  if ($method) { return $method.Invoke($null, @($progId)) }
  if (-not ('ClawOle' -as [type])) {
    Add-Type -Namespace '' -Name 'ClawOle' -MemberDefinition @'
[DllImport("ole32.dll", CharSet = CharSet.Unicode, PreserveSig = false)]
public static extern void CLSIDFromProgID(string progId, out Guid clsid);
[DllImport("oleaut32.dll", PreserveSig = false)]
public static extern void GetActiveObject(ref Guid clsid, IntPtr reserved, [MarshalAs(UnmanagedType.IUnknown)] out object unknown);
'@
  }
  $clsid = [guid]::Empty
  [ClawOle]::CLSIDFromProgID($progId, [ref]$clsid)
  $object = $null
  [ClawOle]::GetActiveObject([ref]$clsid, [IntPtr]::Zero, [ref]$object)
  return $object
}

while ($true) {
  $line = [Console]::In.ReadLine()
  if ($null -eq $line) { break }
  $response = $null
  try {
    $request = $line | ConvertFrom-Json
    $code = [System.Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($request.code))
    $output = @(. ([scriptblock]::Create($code)))
    $value = if ($output.Count -eq 0) { $null } elseif ($output.Count -eq 1) { ConvertTo-ClawValue $output[0] } else { ,@($output | Select-Object -First 200 | ForEach-Object { ConvertTo-ClawValue $_ }) }
    $response = [ordered]@{ ok = $true; value = $value }
  } catch {
    $message = $_.Exception.Message
    if ($_.Exception.InnerException) { $message = $_.Exception.InnerException.Message }
    $response = [ordered]@{ ok = $false; error = $message.Trim() }
  }
  try { $json = ConvertTo-Json -InputObject $response -Depth 8 -Compress }
  catch { $json = ConvertTo-Json -InputObject ([ordered]@{ ok = $false; error = "result not convertible: $($_.Exception.Message)" }) -Compress }
  [Console]::Out.WriteLine($MARKER_PLACEHOLDER + $json)
  [Console]::Out.Flush()
}
"#;

#[derive(Debug, Deserialize)]
pub(crate) struct ComInput {
    action: String,
    /// The object's name in the session, e.g. `excel`.
    #[serde(alias = "name", alias = "handle")]
    object: Option<String>,
    /// e.g. `Excel.Application`.
    #[serde(alias = "progid", alias = "progId")]
    prog_id: Option<String>,
    /// Member path after the object, e.g. `ActiveSheet.Range("A1").Value2`.
    path: Option<String>,
    /// For `set`.
    value: Option<Value>,
    /// For `call`.
    #[serde(default)]
    args: Vec<Value>,
    /// For `create`: show the application window (default true).
    visible: Option<bool>,
    /// For `release`: call `Quit()` first.
    quit: Option<bool>,
    /// For `run`: PowerShell run in the session.
    script: Option<String>,
    /// For `progids`: part of the ProgID.
    filter: Option<String>,
    /// Seconds (default 120).
    timeout: Option<u64>,
}

pub(crate) fn run(input: ComInput) -> Result<Value, String> {
    let code = script_for(&input)?;
    let timeout = input.timeout.map_or(DEFAULT_TIMEOUT, |seconds| {
        Duration::from_secs(seconds.clamp(1, 1800))
    });
    let reply = session_call(&code, timeout)?;
    Ok(reply)
}

/// The PowerShell one action runs.
fn script_for(input: &ComInput) -> Result<String, String> {
    let object = || -> Result<String, String> {
        let name = input
            .object
            .as_deref()
            .map(str::trim)
            .ok_or("this action needs `object` (the name given at create/attach)")?;
        valid_name(name)?;
        Ok(format!("$global:{name}"))
    };
    let path = || -> Result<String, String> {
        match input
            .path
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty())
        {
            Some(path) => {
                valid_path(path)?;
                Ok(format!(".{}", path.trim_start_matches('.')))
            }
            None => Ok(String::new()),
        }
    };
    let prog_id = || -> Result<String, String> {
        let prog_id = input
            .prog_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .ok_or("this action needs `prog_id`, e.g. Excel.Application")?;
        Ok(literal(&Value::String(prog_id.to_string())))
    };
    Ok(match input.action.as_str() {
        "create" => {
            let target = object()?;
            let visible = if input.visible.unwrap_or(true) {
                format!("try {{ {target}.Visible = $true }} catch {{}}; ")
            } else {
                String::new()
            };
            format!("{target} = New-Object -ComObject {}; {visible}{target}", prog_id()?)
        }
        "attach" => {
            let target = object()?;
            format!("{target} = Get-ClawActiveObject {}; {target}", prog_id()?)
        }
        "get" => format!("{}{}", object()?, path()?),
        "set" => {
            let path = path()?;
            if path.is_empty() {
                return Err("set needs `path`, e.g. ActiveSheet.Range(\"A1\").Value2".to_string());
            }
            let value = input.value.as_ref().ok_or("set needs `value`")?;
            let target = format!("{}{path}", object()?);
            format!("{target} = {}; {target}", literal_for_set(value))
        }
        "call" => {
            let path = path()?;
            if path.is_empty() {
                return Err("call needs `path`: the method, e.g. Workbooks.Open".to_string());
            }
            let args: Vec<String> = input.args.iter().map(literal_for_set).collect();
            format!("{}{path}({})", object()?, args.join(", "))
        }
        "inspect" => format!(
            "Get-Member -InputObject ({}{}) | Where-Object {{ $_.MemberType -notmatch 'Event' }} | Select-Object -First 300 | \
             ForEach-Object {{ [ordered]@{{ name = $_.Name; kind = \"$($_.MemberType)\"; signature = (\"$($_.Definition)\" -replace '\\s+', ' ').Substring(0, [Math]::Min(160, (\"$($_.Definition)\").Length)) }} }}",
            object()?,
            path()?
        ),
        "release" => {
            let target = object()?;
            let quit = if input.quit.unwrap_or(false) {
                format!("try {{ [void]{target}.Quit() }} catch {{}}; ")
            } else {
                String::new()
            };
            let name = input.object.as_deref().unwrap_or_default().trim();
            format!(
                "{quit}try {{ [void][System.Runtime.InteropServices.Marshal]::FinalReleaseComObject({target}) }} catch {{}}; \
                 Remove-Variable -Scope Global -Name '{name}'; [GC]::Collect(); [GC]::WaitForPendingFinalizers(); 'released'"
            )
        }
        "objects" => "Get-Variable -Scope Global | Where-Object { Test-ClawCom $_.Value } | \
             ForEach-Object { [ordered]@{ object = $_.Name; type = (Get-ClawTypeName $_.Value) } }"
            .to_string(),
        "progids" => {
            let filter = input.filter.as_deref().unwrap_or_default();
            format!(
                "Get-ChildItem 'Registry::HKEY_CLASSES_ROOT' -Name | Where-Object {{ $_ -match '^[A-Za-z][\\w-]*\\.[\\w.-]+$' -and $_ -like {} }} | \
                 Where-Object {{ Test-Path \"Registry::HKEY_CLASSES_ROOT\\$_\\CLSID\" }} | Select-Object -First 300",
                literal(&Value::String(format!("*{filter}*")))
            )
        }
        "run" => input
            .script
            .clone()
            .filter(|script| !script.trim().is_empty())
            .ok_or("run needs `script` (PowerShell; objects are $global:<name>)")?,
        other => {
            return Err(format!(
                "unknown action `{other}`; use create, attach, get, set, call, inspect, release, objects, progids or run"
            ))
        }
    })
}

fn valid_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let ok = chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "`{name}` is not a valid object name (letters, digits, _)"
        ))
    }
}

/// A member path: properties, methods and indexers, nothing that would end
/// the expression (the `run` action is there for scripts).
fn valid_path(path: &str) -> Result<(), String> {
    let mut quote: Option<char> = None;
    for ch in path.chars() {
        match quote {
            Some(open) if ch == open => quote = None,
            Some(_) => {}
            None if ch == '\'' || ch == '"' => quote = Some(ch),
            None if matches!(
                ch,
                ';' | '|' | '&' | '`' | '$' | '{' | '}' | '\n' | '\r' | '#' | '<' | '>'
            ) =>
            {
                return Err(format!(
                    "`{ch}` is not allowed in a path; use the `run` action for scripts"
                ))
            }
            None => {}
        }
    }
    if quote.is_some() || path.contains(['\n', '\r', '`', '$']) {
        return Err(
            "unbalanced quote or `$`/backtick in path; use the `run` action for scripts"
                .to_string(),
        );
    }
    Ok(())
}

/// A PowerShell literal for a JSON value.
fn literal(value: &Value) -> String {
    match value {
        Value::Null => "$null".to_string(),
        Value::Bool(flag) => if *flag { "$true" } else { "$false" }.to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => format!(
            "'{}'",
            text.replace('\'', "''")
                .replace(['\u{2018}', '\u{2019}'], "''")
        ),
        Value::Array(items) => format!(
            "@({})",
            items.iter().map(literal).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(map) => format!(
            "[ordered]@{{ {} }}",
            map.iter()
                .map(|(key, value)| format!(
                    "{} = {}",
                    literal(&Value::String(key.clone())),
                    literal(value)
                ))
                .collect::<Vec<_>>()
                .join("; ")
        ),
    }
}

/// Like [`literal`], but rows of values (`[[1, 2], [3, 4]]`) become a 2-D
/// array, which is what COM methods such as Excel's `Range.Value2` take.
fn literal_for_set(value: &Value) -> String {
    match value {
        Value::Array(rows) if !rows.is_empty() && rows.iter().all(Value::is_array) => {
            format!("(New-Claw2D {})", literal(value))
        }
        _ => literal(value),
    }
}

// ------------------------------------------------------------- the session

struct Session {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    stderr: Arc<Mutex<Vec<String>>>,
}

static SESSION: Mutex<Option<Session>> = Mutex::new(None);

/// The PowerShell to run the session with, and its arguments before the
/// script.
fn powershell() -> Result<(String, Vec<&'static str>), String> {
    let common = vec![
        "-NoLogo",
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
    ];
    if let Ok(program) = std::env::var("CLAW_COM_POWERSHELL") {
        let mut args = common;
        if cfg!(windows) {
            args.push("-STA");
        }
        return Ok((program, args));
    }
    let candidates: &[&str] = if cfg!(windows) {
        &["powershell.exe", "pwsh.exe"]
    } else {
        // WSL: the Windows side's PowerShell.
        &["powershell.exe"]
    };
    for program in candidates {
        let found = Command::new(program)
            .args(["-NoLogo", "-NoProfile", "-Command", "exit 0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if found {
            let mut args = common;
            args.push("-STA");
            return Ok(((*program).to_string(), args));
        }
    }
    Err(
        "COM automation needs Windows (or WSL with powershell.exe on the PATH); \
         no Windows PowerShell was found"
            .to_string(),
    )
}

fn start() -> Result<Session, String> {
    let (program, mut args) = powershell()?;
    let script = HOST_SCRIPT.replace("$MARKER_PLACEHOLDER", &format!("'{MARKER}'"));
    let encoded = base64(
        &script
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<u8>>(),
    );
    args.push("-EncodedCommand");
    let mut child = Command::new(&program)
        .args(&args)
        .arg(&encoded)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot start {program}: {error}"))?;
    let stdin = child.stdin.take().ok_or("PowerShell has no stdin")?;
    let stdout = child.stdout.take().ok_or("PowerShell has no stdout")?;
    let stderr_pipe = child.stderr.take().ok_or("PowerShell has no stderr")?;
    let (sender, lines) = channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    let stderr = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&stderr);
    std::thread::spawn(move || {
        for line in BufReader::new(stderr_pipe).lines() {
            let Ok(line) = line else { break };
            let mut kept = sink.lock().unwrap_or_else(PoisonError::into_inner);
            kept.push(line);
            let excess = kept.len().saturating_sub(40);
            kept.drain(..excess);
        }
    });
    Ok(Session {
        child,
        stdin,
        lines,
        stderr,
    })
}

/// Runs `code` in the session (starting it if needed) and returns its value
/// and anything it printed.
fn session_call(code: &str, timeout: Duration) -> Result<Value, String> {
    let mut guard = SESSION.lock().unwrap_or_else(PoisonError::into_inner);
    let alive = guard
        .as_mut()
        .is_some_and(|session| matches!(session.child.try_wait(), Ok(None)));
    if !alive {
        *guard = Some(start()?);
    }
    let session = guard.as_mut().ok_or("no COM session")?;
    // Output left over from an earlier, abandoned call.
    while session.lines.try_recv().is_ok() {}
    let request = json!({ "code": base64(code.as_bytes()) });
    let sent = writeln!(session.stdin, "{request}").and_then(|()| session.stdin.flush());
    if let Err(error) = sent {
        *guard = None;
        return Err(format!(
            "the COM session ended ({error}); try again to start a new one"
        ));
    }
    let deadline = Instant::now() + timeout;
    let mut printed = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match session.lines.recv_timeout(left) {
            Ok(line) => {
                if let Some(result) = line.strip_prefix(MARKER) {
                    let mut reply: Value = serde_json::from_str(result)
                        .map_err(|error| format!("unreadable reply from PowerShell: {error}"))?;
                    if !printed.is_empty() {
                        reply["printed"] = json!(printed);
                    }
                    return if reply["ok"] == json!(true) {
                        reply.as_object_mut().map(|map| map.remove("ok"));
                        Ok(reply)
                    } else {
                        Err(reply["error"]
                            .as_str()
                            .unwrap_or("the call failed")
                            .to_string())
                    };
                }
                if printed.len() < MAX_PRINTED {
                    printed.push(line);
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if let Some(mut session) = guard.take() {
                    let _ = session.child.kill();
                    let _ = session.child.wait();
                }
                return Err(format!(
                    "no answer after {}s (a dialog box in the application may be waiting for the user). \
                     The COM session was restarted, so its objects are gone; the applications keep \
                     running and `attach` reconnects to them",
                    timeout.as_secs()
                ));
            }
            Err(RecvTimeoutError::Disconnected) => {
                let stderr = guard
                    .take()
                    .map(|session| {
                        session
                            .stderr
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .join("\n")
                    })
                    .unwrap_or_default();
                return Err(format!("the PowerShell session exited: {}", stderr.trim()));
            }
        }
    }
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> shift) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{literal, literal_for_set, script_for, valid_path, ComInput};
    use serde_json::json;

    fn input(value: serde_json::Value) -> ComInput {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn actions_become_powershell() {
        let create = script_for(&input(
            json!({"action": "create", "object": "excel", "prog_id": "Excel.Application"}),
        ))
        .unwrap();
        assert_eq!(
            create,
            "$global:excel = New-Object -ComObject 'Excel.Application'; try { $global:excel.Visible = $true } catch {}; $global:excel"
        );
        let set = script_for(&input(json!({
            "action": "set", "object": "excel",
            "path": "ActiveSheet.Range(\"A1:B2\").Value2", "value": [[1, "a"], [2, "b's"]]
        })))
        .unwrap();
        assert!(set.starts_with("$global:excel.ActiveSheet.Range(\"A1:B2\").Value2 = (New-Claw2D @(@(1, 'a'), @(2, 'b''s')))"));
        let call = script_for(&input(json!({
            "action": "call", "object": "word", "path": "Documents.Open",
            "args": ["C:\\a.docx", false]
        })))
        .unwrap();
        assert_eq!(call, "$global:word.Documents.Open('C:\\a.docx', $false)");
    }

    #[test]
    fn paths_cannot_smuggle_commands_and_names_are_checked() {
        assert!(valid_path("Range(\"A1;B2\").Value2").is_ok());
        assert!(valid_path("Quit(); Remove-Item x").is_err());
        assert!(valid_path("Name | Out-File x").is_err());
        assert!(valid_path("Item($x)").is_err());
        assert!(script_for(&input(json!({"action": "get", "object": "a b"}))).is_err());
        assert_eq!(literal(&json!(null)), "$null");
        assert_eq!(literal_for_set(&json!([1, 2])), "@(1, 2)");
    }

    /// The session protocol, with `pwsh` standing in for Windows PowerShell
    /// (no COM there). Run with `--ignored` where pwsh is installed.
    #[test]
    #[ignore = "needs pwsh"]
    fn session_keeps_state_and_reports_errors() {
        std::env::set_var("CLAW_COM_POWERSHELL", "pwsh");
        let run = |value| super::run(input(value));
        let first = run(json!({"action": "run", "script": "$global:counter = 41; Write-Host 'hi'; $global:counter + 1"})).unwrap();
        assert_eq!(first["value"], json!(42));
        assert_eq!(first["printed"], json!(["hi"]));
        let rows =
            run(json!({"action": "run", "script": "$g = New-Claw2D @(@(1,'x'),@(2,'y')); ,$g"}))
                .unwrap();
        assert_eq!(rows["value"], json!([[1, "x"], [2, "y"]]));
        let again = run(json!({"action": "run", "script": "$global:counter"})).unwrap();
        assert_eq!(again["value"], json!(41));
        let error = run(json!({"action": "run", "script": "throw 'boom'"})).unwrap_err();
        assert!(error.contains("boom"), "{error}");
        let slow =
            run(json!({"action": "run", "script": "Start-Sleep 5", "timeout": 1})).unwrap_err();
        assert!(slow.contains("no answer"), "{slow}");
        let fresh = run(json!({"action": "run", "script": "$null -eq $global:counter"})).unwrap();
        assert_eq!(fresh["value"], json!(true));
        // The generated actions, on a .NET object standing in for a COM one.
        run(json!({"action": "run", "script": "$global:list = [System.Collections.ArrayList]::new(); $null"})).unwrap();
        run(json!({"action": "call", "object": "list", "path": "Add", "args": ["it's"]})).unwrap();
        let count = run(json!({"action": "get", "object": "list", "path": "Count"})).unwrap();
        assert_eq!(count["value"], json!(1));
        let item = run(json!({"action": "get", "object": "list", "path": "Item(0)"})).unwrap();
        assert_eq!(item["value"], json!("it's"));
        run(json!({"action": "set", "object": "list", "path": "Capacity", "value": 16})).unwrap();
        let members = run(json!({"action": "inspect", "object": "list"})).unwrap();
        assert!(members["value"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["name"] == "Add"));
        let objects = run(json!({"action": "objects"})).unwrap();
        assert!(
            objects["value"].is_null()
                || objects["value"].is_array()
                || objects["value"].is_object()
        );
    }
}
