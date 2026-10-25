#!/usr/bin/env bash
# SDK: platforms;android-36 and build-tools;36.0.0. NDK r28+ (16 KB page support).
# cargo install cargo-ndk --locked; rustup target add aarch64-linux-android
# ANDROID_HOME=/path/to/sdk ANDROID_NDK_HOME=/path/to/ndk bash packaging/android/build.sh
# The output is a locally signed preview APK, not a Google Play release.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo"
abi="${1:-arm64-v8a}"
case "$abi" in
    arm64-v8a) target=aarch64-linux-android ;;
    x86_64) target=x86_64-linux-android ;;
    *) echo 'Choose arm64-v8a (tablets) or x86_64 (emulator)' >&2; exit 2 ;;
esac
sdk="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-}}"
: "${sdk:?Set ANDROID_HOME to the Android SDK directory}"
: "${ANDROID_NDK_HOME:?Set ANDROID_NDK_HOME to an NDK r28 or newer directory}"
tools="$sdk/build-tools/${CADCRAFT_ANDROID_BUILD_TOOLS:-36.0.0}"
android_jar="$sdk/platforms/android-36/android.jar"
output="${CADCRAFT_ANDROID_OUTPUT:-$repo/dist/android}"
stage="$output/stage-$abi"
mkdir -p "$stage/classes" "$stage/dex" "$stage/res/mipmap-xxxhdpi" "$stage/lib"
cp -R packaging/android/res/. "$stage/res/"
cp assets/app-icon/cadcraft-256.png "$stage/res/mipmap-xxxhdpi/cadcraft.png"

# winit 0.30.13 drops Android HOVER_* motion events. Patch its locked source in a
# disposable build workspace until upstream exposes those events to the tablet UI.
cargo fetch --locked --target "$target"
workspace="$(python3 packaging/android/prepare_winit.py "$stage")"
(
    cd "$workspace"
    cargo ndk --platform 26 --target "$abi" --output-dir "$stage/lib" build --locked --lib -p cadcraft-android --profile android
)
# Preserve symbols from the exact linked library before stripping the installed copy.
# Keep them with this APK so native crash offsets can be resolved after a workspace expires.
symbols="$output/symbols/$abi"
mkdir -p "$symbols"
cp "$stage/lib/$abi/libcadcraft_android.so" "$symbols/libcadcraft_android.so"
"$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-strip" --strip-unneeded "$stage/lib/$abi/libcadcraft_android.so"
java --module jdk.compiler/com.sun.tools.javac.Main --release 8 -classpath "$android_jar" -d "$stage/classes" \
    packaging/android/java/io/github/storytold/cadcraft/*.java
mapfile -t classes < <(find "$stage/classes" -name '*.class' -print)
"$tools/d8" --min-api 26 --lib "$android_jar" --output "$stage/dex" "${classes[@]}"
"$tools/aapt2" compile --dir "$stage/res" -o "$stage/resources.zip"
"$tools/aapt2" link -o "$stage/base.apk" -I "$android_jar" --manifest packaging/android/AndroidManifest.xml "$stage/resources.zip"

python3 - "$stage" <<'PY'
import pathlib, sys, zipfile
stage = pathlib.Path(sys.argv[1])
# The manifest extracts native libraries, so compression reduces download/install size.
with zipfile.ZipFile(stage / 'base.apk', 'a', compression=zipfile.ZIP_DEFLATED, compresslevel=6) as apk:
    apk.write(stage / 'dex/classes.dex', 'classes.dex')
    for lib in (stage / 'lib').rglob('*.so'):
        apk.write(lib, 'lib/' + str(lib.relative_to(stage / 'lib')))
PY
"$tools/zipalign" -P 16 -f 4 "$stage/base.apk" "$stage/aligned.apk"
keystore="${CADCRAFT_ANDROID_KEYSTORE:-$output/preview.keystore}"
if [[ ! -f "$keystore" ]]; then
    keytool -genkeypair -keystore "$keystore" -storepass android -keypass android \
        -alias androiddebugkey -keyalg RSA -keysize 2048 -validity 10000 -dname 'CN=CADCraft Tablet Preview' -noprompt
fi
apk="$output/CADCraft-Tablet-Preview-$abi.apk"
"$tools/apksigner" sign --ks "$keystore" --ks-key-alias androiddebugkey --ks-pass pass:android --key-pass pass:android \
    --out "$apk" "$stage/aligned.apk"
"$tools/apksigner" verify --verbose "$apk"
"$tools/zipalign" -c -P 16 4 "$apk"
echo "Built $apk ($target)"
