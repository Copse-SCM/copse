# Helpers that work the same on macOS (BSD tools) and Linux (GNU tools).
# Every test suite loads this first.

# Sort and compare like the C library, whatever the machine's language settings.
export LC_ALL=C

# Edit a file in place: sed -i (GNU) or sed -i '' (BSD).
sedi() {
  if sed --version >/dev/null 2>&1; then sed -i "$@"; else sed -i '' "$@"; fi
}

# A file's modification time, in seconds since the epoch.
mtime() {
  stat -c %Y "$1" 2>/dev/null || stat -f %m "$1"
}

# A timestamp N days ago in touch -t format (YYYYMMDDhhmm).
days_ago() {
  date -d "$1 days ago" +%Y%m%d%H%M 2>/dev/null || date -v-"$1"d +%Y%m%d%H%M
}

# Run a command and print the most memory it used, in whole MB.
peak_mb() {
  python3 - "$@" <<'EOF'
import resource, subprocess, sys
subprocess.run(sys.argv[1:], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
peak = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
# macOS reports bytes, Linux kilobytes.
print(peak // (1 << 20) if sys.platform == "darwin" else peak // 1024)
EOF
}

# The SHA-256 of a file, as hex.
sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

# A file's size in bytes, and its permission bits.
fsize() { stat -c %s "$1" 2>/dev/null || stat -f %z "$1"; }
fmode() { stat -c %a "$1" 2>/dev/null || stat -f %Lp "$1"; }

# The SHA-256 of standard input, as hex.
hashin() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum | cut -c1-64; else shasum -a 256 | cut -c1-64; fi
}

# The bytes of all the files under a folder, in KB (what's stored, not disk blocks:
# Linux filesystems charge a whole 4 KB block even for a tiny file or empty folder).
bytes_in() {
  python3 -c "import os,sys; print(sum(os.path.getsize(os.path.join(d, f)) for d, _, fs in os.walk(sys.argv[1]) for f in fs) // 1024)" "$1"
}
