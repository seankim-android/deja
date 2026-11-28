#!/usr/bin/env bash
# Zero-Gradle reproducible build script for the Déjà Android UiAutomator2 HTTP agent APK.
#
# Usage:
#   build-apk.sh --out <path/to/deja-android-agent.apk> \
#     [--sdk-root <android-sdk-dir>] \
#     [--build-tools <version-or-dir>] \
#     [--platform <platform-or-dir>] \
#     [--keystore <persistent-keystore-path>]

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

SDK_ROOT="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-}}"
BUILD_TOOLS_ARG=""
PLATFORM_ARG=""
KEYSTORE_ARG=""
OUT_PATH=""

usage() {
  cat >&2 <<'EOF'
Usage: build-apk.sh --out <apk-path> [--sdk-root <dir>] [--build-tools <ver-or-dir>] [--platform <ver-or-dir>] [--keystore <path>]
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --sdk-root)
      [[ $# -ge 2 ]] || { usage; exit 2; }
      SDK_ROOT="$2"
      shift 2
      ;;
    --build-tools)
      [[ $# -ge 2 ]] || { usage; exit 2; }
      BUILD_TOOLS_ARG="$2"
      shift 2
      ;;
    --platform)
      [[ $# -ge 2 ]] || { usage; exit 2; }
      PLATFORM_ARG="$2"
      shift 2
      ;;
    --keystore)
      [[ $# -ge 2 ]] || { usage; exit 2; }
      KEYSTORE_ARG="$2"
      shift 2
      ;;
    --out)
      [[ $# -ge 2 ]] || { usage; exit 2; }
      OUT_PATH="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "build-apk.sh: unknown argument: $1" >&2
      usage
      exit 2
      ;;
  esac
done

if [[ -z "$OUT_PATH" ]]; then
  echo "build-apk.sh: --out <apk-path> is required" >&2
  usage
  exit 2
fi

if [[ -z "$SDK_ROOT" ]]; then
  if [[ -d "${HOME:-}/Library/Android/sdk" ]]; then
    SDK_ROOT="${HOME}/Library/Android/sdk"
  elif [[ -d "${HOME:-}/Android/Sdk" ]]; then
    SDK_ROOT="${HOME}/Android/Sdk"
  fi
fi

if [[ -z "$SDK_ROOT" || ! -d "$SDK_ROOT" ]]; then
  echo "build-apk.sh: Android SDK root not found (pass --sdk-root or set ANDROID_HOME)" >&2
  exit 1
fi

pick_latest_dir() {
  local base="$1"
  local best=""
  if [[ ! -d "$base" ]]; then
    return 1
  fi
  best="$(ls -1 "$base" 2>/dev/null | sort -V | tail -n 1 || true)"
  if [[ -z "$best" ]]; then
    return 1
  fi
  printf '%s/%s\n' "$base" "$best"
}

if [[ -n "$BUILD_TOOLS_ARG" ]]; then
  if [[ -d "$BUILD_TOOLS_ARG" ]]; then
    BUILD_TOOLS_DIR="$BUILD_TOOLS_ARG"
  else
    BUILD_TOOLS_DIR="$SDK_ROOT/build-tools/$BUILD_TOOLS_ARG"
  fi
else
  BUILD_TOOLS_DIR="$(pick_latest_dir "$SDK_ROOT/build-tools" || true)"
fi

if [[ -z "${BUILD_TOOLS_DIR:-}" || ! -d "$BUILD_TOOLS_DIR" ]]; then
  echo "build-apk.sh: Android build-tools directory not found under $SDK_ROOT/build-tools" >&2
  exit 1
fi

if [[ -n "$PLATFORM_ARG" ]]; then
  if [[ -f "$PLATFORM_ARG" ]]; then
    ANDROID_JAR="$PLATFORM_ARG"
  elif [[ -d "$PLATFORM_ARG" ]]; then
    ANDROID_JAR="$PLATFORM_ARG/android.jar"
  else
    ANDROID_JAR="$SDK_ROOT/platforms/$PLATFORM_ARG/android.jar"
  fi
else
  PLATFORM_DIR="$(pick_latest_dir "$SDK_ROOT/platforms" || true)"
  ANDROID_JAR="${PLATFORM_DIR:+$PLATFORM_DIR/android.jar}"
fi

if [[ -z "${ANDROID_JAR:-}" || ! -f "$ANDROID_JAR" ]]; then
  echo "build-apk.sh: android.jar not found under $SDK_ROOT/platforms" >&2
  exit 1
fi

AAPT2="$BUILD_TOOLS_DIR/aapt2"
D8="$BUILD_TOOLS_DIR/d8"
ZIPALIGN="$BUILD_TOOLS_DIR/zipalign"
APKSIGNER="$BUILD_TOOLS_DIR/apksigner"

for tool in "$AAPT2" "$D8" "$ZIPALIGN" "$APKSIGNER"; do
  if [[ ! -x "$tool" ]]; then
    echo "build-apk.sh: required Android build-tools binary missing or not executable: $tool" >&2
    exit 1
  fi
done

JAVAC_BIN="${JAVA_HOME:+$JAVA_HOME/bin/javac}"
if [[ -z "$JAVAC_BIN" || ! -x "$JAVAC_BIN" ]]; then
  JAVAC_BIN="$(command -v javac || true)"
fi
KEYTOOL_BIN="${JAVA_HOME:+$JAVA_HOME/bin/keytool}"
if [[ -z "$KEYTOOL_BIN" || ! -x "$KEYTOOL_BIN" ]]; then
  KEYTOOL_BIN="$(command -v keytool || true)"
fi
JAR_BIN="${JAVA_HOME:+$JAVA_HOME/bin/jar}"
if [[ -z "$JAR_BIN" || ! -x "$JAR_BIN" ]]; then
  JAR_BIN="$(command -v jar || true)"
fi

if [[ -z "$JAVAC_BIN" || ! -x "$JAVAC_BIN" ]]; then
  echo "build-apk.sh: javac not found (install a JDK or set JAVA_HOME)" >&2
  exit 1
fi
if [[ -z "$KEYTOOL_BIN" || ! -x "$KEYTOOL_BIN" ]]; then
  echo "build-apk.sh: keytool not found (install a JDK or set JAVA_HOME)" >&2
  exit 1
fi

TMP_WORK="$(mktemp -d "${TMPDIR:-/tmp}/deja-android-agent-build.XXXXXX")"
cleanup() {
  rm -rf "$TMP_WORK"
}
trap cleanup EXIT INT TERM

mkdir -p "$TMP_WORK/classes" "$TMP_WORK/dex"
OUT_DIR="$(dirname "$OUT_PATH")"
mkdir -p "$OUT_DIR"

# 1. Link compiled binary AndroidManifest.xml into a base APK archive.
"$AAPT2" link \
  -o "$TMP_WORK/unaligned.apk" \
  -I "$ANDROID_JAR" \
  --manifest "$SCRIPT_DIR/AndroidManifest.xml" \
  --min-sdk-version 24 \
  --target-sdk-version 36

# 2. Compile Java sources directly against android.jar (zero Gradle, zero third-party JARs).
"$JAVAC_BIN" \
  --release 11 \
  -Xlint:none \
  -classpath "$ANDROID_JAR" \
  -d "$TMP_WORK/classes" \
  "$SCRIPT_DIR/src/dev/deja/agent/DejaAgentInstrumentation.java" \
  "$SCRIPT_DIR/src/dev/deja/agent/HierarchySnapshot.java" \
  "$SCRIPT_DIR/src/dev/deja/agent/UiActions.java" \
  "$SCRIPT_DIR/src/dev/deja/agent/HttpServer.java"

# 3. Dex compiled class files targeting min-api 24.
CLASS_FILES=()
while IFS= read -r -d '' f; do
  CLASS_FILES+=("$f")
done < <(find "$TMP_WORK/classes" -type f -name '*.class' -print0 | sort -z)

if [[ ${#CLASS_FILES[@]} -eq 0 ]]; then
  echo "build-apk.sh: javac produced no .class files" >&2
  exit 1
fi

"$D8" \
  --min-api 24 \
  --lib "$ANDROID_JAR" \
  --output "$TMP_WORK/dex" \
  "${CLASS_FILES[@]}"

# 4. Pack classes.dex into the APK archive.
if command -v zip >/dev/null 2>&1; then
  (cd "$TMP_WORK/dex" && zip -q -X "$TMP_WORK/unaligned.apk" classes.dex)
elif [[ -n "$JAR_BIN" && -x "$JAR_BIN" ]]; then
  "$JAR_BIN" uf "$TMP_WORK/unaligned.apk" -C "$TMP_WORK/dex" classes.dex
else
  echo "build-apk.sh: neither zip nor jar is available to package classes.dex" >&2
  exit 1
fi

# 5. Zip-align on 4-byte boundaries (page-aligned for shared libraries).
"$ZIPALIGN" -f -p 4 "$TMP_WORK/unaligned.apk" "$TMP_WORK/aligned.apk"

# 6. Reuse persistent debug keystore when `--keystore` is supplied (created once with 0600),
#    or generate an ephemeral keystore inside TMP_WORK (deleted on trap).
if [[ -n "$KEYSTORE_ARG" ]]; then
  KEYSTORE="$KEYSTORE_ARG"
  mkdir -p "$(dirname "$KEYSTORE")"
  if [[ ! -f "$KEYSTORE" ]]; then
    TMP_KS="$TMP_WORK/new-debug.keystore"
    "$KEYTOOL_BIN" -genkeypair \
      -keystore "$TMP_KS" \
      -storepass android \
      -alias androiddebugkey \
      -keypass android \
      -keyalg RSA \
      -keysize 2048 \
      -validity 10000 \
      -dname "CN=Deja Debug,O=Deja,C=US" \
      >/dev/null 2>&1
    chmod 600 "$TMP_KS"
    if ! mv -n "$TMP_KS" "$KEYSTORE" 2>/dev/null; then
      if [[ ! -f "$KEYSTORE" ]]; then
        mv "$TMP_KS" "$KEYSTORE"
      fi
    fi
    chmod 600 "$KEYSTORE"
  fi
else
  KEYSTORE="$TMP_WORK/debug.keystore"
  "$KEYTOOL_BIN" -genkeypair \
    -keystore "$KEYSTORE" \
    -storepass android \
    -alias androiddebugkey \
    -keypass android \
    -keyalg RSA \
    -keysize 2048 \
    -validity 10000 \
    -dname "CN=Deja Debug,O=Deja,C=US" \
    >/dev/null 2>&1
fi

# 7. Sign (v1/v2/v3) and verify the resulting APK.
"$APKSIGNER" sign \
  --ks "$KEYSTORE" \
  --ks-key-alias androiddebugkey \
  --ks-pass pass:android \
  --key-pass pass:android \
  --out "$OUT_PATH" \
  "$TMP_WORK/aligned.apk"

"$APKSIGNER" verify "$OUT_PATH"
