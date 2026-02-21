import "wrun/args" for Args
import "wrun/env" for Env
import "wrun/process" for Process, Shell
import "wrun/print" for Log
import "wrun/str" for Str

class SpacebotDeploy {
    static usage() {
        System.print("Deploy/stop spacebot locally or on a remote host")
        System.print("")
        System.print("Usage:")
        System.print("  wrun spacebot_deploy.wren <command> [options]")
        System.print("")
        System.print("Commands:")
        System.print("  deploy-macmini    Build locally and deploy to remote host over SSH")
        System.print("  stop-macmini      Stop daemon/service on remote host over SSH")
        System.print("  deploy-local      Build and deploy on the current machine (no SSH)")
        System.print("  stop-local        Stop daemon/service on the current machine (no SSH)")
        System.print("")
        System.print("Options:")
        System.print("  --host=...             SSH host alias (default: mini)")
        System.print("  --service-label=...    launchd label override (auto-detected if omitted)")
        System.print("  --binary=...           binary name (default: spacebot)")
        System.print("  -h, --help             Show this help")
        System.print("")
        System.print("Examples:")
        System.print("  wrun spacebot_deploy.wren deploy-macmini")
        System.print("  wrun spacebot_deploy.wren deploy-macmini --host=mini")
        System.print("  wrun spacebot_deploy.wren deploy-local")
        System.print("  wrun spacebot_deploy.wren stop-local --service-label=com.example.spacebot")
    }

    static shellQuote(value) {
        var escaped = Str.replaceAll(value, "'", """'"'"'""")
        return "'%(escaped)'"
    }

    static runStep(label, command) {
        Log.info(label, {"command": command})
        var code = Shell.interactive(command)
        if (code != 0) {
            Log.error("Step failed", {"label": label, "exitCode": code})
            return false
        }
        return true
    }

    static stopScript(binaryName, serviceLabel) {
        var template = """set -euo pipefail
label=__SERVICE_LABEL__
if [ -z "$label" ]; then
  label=$(launchctl list | awk 'tolower($3) ~ /spacebot/ {print $3; exit}')
fi
if [ -z "$label" ] && [ -d "$HOME/Library/LaunchAgents" ]; then
  plist_guess=$(find "$HOME/Library/LaunchAgents" -maxdepth 1 -type f -name "*.plist" 2>/dev/null | awk 'tolower($0) ~ /spacebot/ {print; exit}')
  if [ -n "$plist_guess" ]; then
    label=$(basename "$plist_guess" .plist)
  fi
fi
if [ -z "$label" ] && [ -d "/Library/LaunchDaemons" ]; then
  plist_guess=$(find "/Library/LaunchDaemons" -maxdepth 1 -type f -name "*.plist" 2>/dev/null | awk 'tolower($0) ~ /spacebot/ {print; exit}')
  if [ -n "$plist_guess" ]; then
    label=$(basename "$plist_guess" .plist)
  fi
fi
if [ -z "$label" ]; then
  echo "Could not determine launchd label containing 'spacebot'." >&2
  exit 1
fi

uid="$(id -u)"
target="gui/$uid/$label"
for candidate in "$target" "user/$uid/$label" "system/$label" "$label"; do
  if launchctl print "$candidate" >/dev/null 2>&1; then
    target="$candidate"
    break
  fi
done

echo "Stopping launchd service: $target"
launchctl kill SIGTERM "$target" >/dev/null 2>&1 || launchctl stop "$target" >/dev/null 2>&1 || true
launchctl bootout "$target" >/dev/null 2>&1 || true

echo "Stopping __BINARY_NAME__ daemon if running"
~/.cargo/bin/__BINARY_NAME__ stop >/dev/null 2>&1 || true

pid=""
if [ -f ~/.spacebot/spacebot.pid ]; then
  pid="$(cat ~/.spacebot/spacebot.pid 2>/dev/null || true)"
fi

if [ -n "$pid" ] && kill -0 "$pid" >/dev/null 2>&1; then
  kill -TERM "$pid" >/dev/null 2>&1 || true
fi

pkill -TERM -f "/.cargo/bin/__BINARY_NAME__( |$)" >/dev/null 2>&1 || true

for _ in $(seq 1 20); do
  if ! pgrep -f "/.cargo/bin/__BINARY_NAME__( |$)" >/dev/null 2>&1; then
    break
  fi
  sleep 1
done

if pgrep -f "/.cargo/bin/__BINARY_NAME__( |$)" >/dev/null 2>&1; then
  pkill -KILL -f "/.cargo/bin/__BINARY_NAME__( |$)" >/dev/null 2>&1 || true
  sleep 1
fi

if pgrep -f "/.cargo/bin/__BINARY_NAME__( |$)" >/dev/null 2>&1; then
  echo "__BINARY_NAME__ is still running after stop attempts" >&2
  pgrep -fal "/.cargo/bin/__BINARY_NAME__( |$)" >&2 || true
  exit 1
fi

rm -f ~/.spacebot/spacebot.pid ~/.spacebot/spacebot.sock
"""
        var script = Str.replaceAll(template, "__SERVICE_LABEL__", SpacebotDeploy.shellQuote(serviceLabel))
        script = Str.replaceAll(script, "__BINARY_NAME__", binaryName)
        return script
    }

    static startScript(binaryName, serviceLabel) {
        var template = """set -euo pipefail
label=__SERVICE_LABEL__
if [ -z "$label" ]; then
  label=$(launchctl list | awk 'tolower($3) ~ /spacebot/ {print $3; exit}')
fi
if [ -z "$label" ] && [ -d "$HOME/Library/LaunchAgents" ]; then
  plist_guess=$(find "$HOME/Library/LaunchAgents" -maxdepth 1 -type f -name "*.plist" 2>/dev/null | awk 'tolower($0) ~ /spacebot/ {print; exit}')
  if [ -n "$plist_guess" ]; then
    label=$(basename "$plist_guess" .plist)
  fi
fi
if [ -z "$label" ] && [ -d "/Library/LaunchDaemons" ]; then
  plist_guess=$(find "/Library/LaunchDaemons" -maxdepth 1 -type f -name "*.plist" 2>/dev/null | awk 'tolower($0) ~ /spacebot/ {print; exit}')
  if [ -n "$plist_guess" ]; then
    label=$(basename "$plist_guess" .plist)
  fi
fi
if [ -z "$label" ]; then
  echo "Could not determine launchd label containing 'spacebot'." >&2
  exit 1
fi

uid="$(id -u)"
domain="gui/$uid"
target="$domain/$label"
plist="$HOME/Library/LaunchAgents/$label.plist"

if ! launchctl print "$target" >/dev/null 2>&1; then
  if [ -f "$plist" ]; then
    echo "Bootstrapping launchd service: $plist"
    launchctl bootstrap "$domain" "$plist" >/dev/null 2>&1 || true
  fi
fi

echo "Starting launchd service: $target"
launchctl kickstart -k "$target" >/dev/null 2>&1 || launchctl start "$target" >/dev/null 2>&1 || true

is_healthy() {
  if ~/.cargo/bin/__BINARY_NAME__ status >/dev/null 2>&1; then
    return 0
  fi
  if launchctl print "$target" 2>/dev/null | grep -q "state = running"; then
    return 0
  fi
  if pgrep -f "/.cargo/bin/__BINARY_NAME__( |$).*--foreground" >/dev/null 2>&1; then
    return 0
  fi
  return 1
}

for _ in $(seq 1 20); do
  if is_healthy; then
    echo "Spacebot daemon is running"
    if launchctl print "$target" 2>/dev/null | grep -q "state = spawn scheduled"; then
      echo "Warning: launchd target is 'spawn scheduled' (not supervising foreground process)." >&2
      echo "Consider updating $plist to run: __BINARY_NAME__ start --foreground" >&2
    fi
    exit 0
  fi
  sleep 1
done

echo "Launchd start did not produce healthy status, trying direct daemon start"
~/.cargo/bin/__BINARY_NAME__ start >/dev/null 2>&1 || true

for _ in $(seq 1 20); do
  if is_healthy; then
    echo "Spacebot daemon is running (direct-start fallback)"
    if launchctl print "$target" 2>/dev/null | grep -q "state = spawn scheduled"; then
      echo "Warning: launchd target is 'spawn scheduled' (not supervising foreground process)." >&2
      echo "Consider updating $plist to run: __BINARY_NAME__ start --foreground" >&2
    fi
    exit 0
  fi
  sleep 1
done

echo "Spacebot daemon did not report healthy status after restart" >&2
~/.cargo/bin/__BINARY_NAME__ status || true
exit 1
"""
        var script = Str.replaceAll(template, "__SERVICE_LABEL__", SpacebotDeploy.shellQuote(serviceLabel))
        script = Str.replaceAll(script, "__BINARY_NAME__", binaryName)
        return script
    }

    static remoteRun(host, script) {
        var command = "ssh %(host) %(SpacebotDeploy.shellQuote(script))"
        return Shell.interactive(command)
    }

    static localRun(script) {
        var command = "bash -eu -o pipefail -c %(SpacebotDeploy.shellQuote(script))"
        return Shell.interactive(command)
    }
}

var command = null
var host = "mini"
var binaryName = "spacebot"
var serviceLabel = ""
var showHelp = false

for (i in 0...Args.count()) {
    var arg = Args.get(i)
    if (arg == "-h" || arg == "--help" || arg == "help") {
        showHelp = true
    } else if (Str.startsWith(arg, "--host=")) {
        host = Str.slice(arg, 7)
    } else if (Str.startsWith(arg, "--service-label=")) {
        serviceLabel = Str.slice(arg, 16)
    } else if (Str.startsWith(arg, "--binary=")) {
        binaryName = Str.slice(arg, 9)
    } else if (!Str.startsWith(arg, "--") && command == null) {
        command = arg
    } else if (!Str.startsWith(arg, "--") && host == "mini") {
        host = arg
    } else {
        Log.error("Unknown argument", {"arg": arg})
        SpacebotDeploy.usage()
        Process.exit(1)
    }
}

if (showHelp || command == null) {
    SpacebotDeploy.usage()
    Process.exit(showHelp ? 0 : 1)
}

var localInstallDir = "%(Env.home())/.cargo/bin"
var localBinary = "%(localInstallDir)/%(binaryName)"
var localBinaryNew = "%(localBinary).new"
var buildBinary = "target/release/%(binaryName)"

if (command == "deploy-macmini") {
    if (!SpacebotDeploy.runStep("Build release binary", "cargo build --release --bin %(binaryName)")) Process.exit(1)
    if (SpacebotDeploy.remoteRun(host, SpacebotDeploy.stopScript(binaryName, serviceLabel)) != 0) Process.exit(1)
    if (!SpacebotDeploy.runStep("Ensure remote install dir", "ssh %(host) 'mkdir -p ~/.cargo/bin'")) Process.exit(1)
    if (!SpacebotDeploy.runStep("Copy binary to remote (.new)", "scp %(buildBinary) %(host):~/.cargo/bin/%(binaryName).new")) Process.exit(1)
    if (!SpacebotDeploy.runStep("Activate remote binary", "ssh %(host) 'chmod +x ~/.cargo/bin/%(binaryName).new && mv ~/.cargo/bin/%(binaryName).new ~/.cargo/bin/%(binaryName)'")) Process.exit(1)
    if (SpacebotDeploy.remoteRun(host, SpacebotDeploy.startScript(binaryName, serviceLabel)) != 0) Process.exit(1)
    Log.info("Remote deployment complete", {"host": host, "binary": binaryName})
    Process.exit(0)
}

if (command == "stop-macmini") {
    if (SpacebotDeploy.remoteRun(host, SpacebotDeploy.stopScript(binaryName, serviceLabel)) != 0) Process.exit(1)
    Log.info("Remote stop complete", {"host": host, "binary": binaryName})
    Process.exit(0)
}

if (command == "deploy-local") {
    if (!SpacebotDeploy.runStep("Build release binary", "cargo build --release --bin %(binaryName)")) Process.exit(1)
    if (SpacebotDeploy.localRun(SpacebotDeploy.stopScript(binaryName, serviceLabel)) != 0) Process.exit(1)
    if (!SpacebotDeploy.runStep("Ensure local install dir", "mkdir -p %(SpacebotDeploy.shellQuote(localInstallDir))")) Process.exit(1)
    if (!SpacebotDeploy.runStep("Copy binary to local (.new)", "cp %(SpacebotDeploy.shellQuote(buildBinary)) %(SpacebotDeploy.shellQuote(localBinaryNew))")) Process.exit(1)
    if (!SpacebotDeploy.runStep("Activate local binary", "chmod +x %(SpacebotDeploy.shellQuote(localBinaryNew)) && mv %(SpacebotDeploy.shellQuote(localBinaryNew)) %(SpacebotDeploy.shellQuote(localBinary))")) Process.exit(1)
    if (SpacebotDeploy.localRun(SpacebotDeploy.startScript(binaryName, serviceLabel)) != 0) Process.exit(1)
    Log.info("Local deployment complete", {"binary": binaryName, "path": localBinary})
    Process.exit(0)
}

if (command == "stop-local") {
    if (SpacebotDeploy.localRun(SpacebotDeploy.stopScript(binaryName, serviceLabel)) != 0) Process.exit(1)
    Log.info("Local stop complete", {"binary": binaryName})
    Process.exit(0)
}

Log.error("Unknown command", {"command": command})
SpacebotDeploy.usage()
Process.exit(1)
