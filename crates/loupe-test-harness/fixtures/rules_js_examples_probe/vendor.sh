#!/usr/bin/env bash
# Vendor the probe's transitive bzlmod deps into a directory Loupe can read.
#
# Output: $VENDOR_DIR (default /tmp/loupe-rules-js-vendor) populated with
# every transitively-loaded `external/<canonical>/` repo, the
# `bazel_tools/` symlink, and the auto-synthesised `local_config_platform`.
# Re-runnable.

set -euo pipefail

cd "$(dirname "$0")"

VENDOR_DIR="${VENDOR_DIR:-/tmp/loupe-rules-js-vendor}"
BAZEL_JVM_TRUSTSTORE="${BAZEL_JVM_TRUSTSTORE:-/etc/ssl/certs/java/cacerts}"
BAZEL_JVM_ARGS="--host_jvm_args=-Djavax.net.ssl.trustStore=${BAZEL_JVM_TRUSTSTORE}"

rm -rf "$VENDOR_DIR" MODULE.bazel.lock vendor

bazel "$BAZEL_JVM_ARGS" vendor --vendor_dir=vendor //...
mv vendor "$VENDOR_DIR"

# Bazel's `bazel_tools` lives in the install dir, not the external cache.
BAZEL_INSTALL=$(bazel "$BAZEL_JVM_ARGS" info install_base)
ln -sfn "$BAZEL_INSTALL/embedded_tools" "$VENDOR_DIR/bazel_tools"

# Pull repos `bazel vendor //...` doesn't fetch but Loupe's MODULE.bazel
# walk reaches: `local_config_platform` (Bazel auto-synthesised at the
# start of each build, never user-vendored).
EXTERNAL_CACHE=$(bazel "$BAZEL_JVM_ARGS" info output_base)/external
for repo in local_config_platform; do
    if [[ -d "$EXTERNAL_CACHE/$repo" && ! -d "$VENDOR_DIR/$repo" ]]; then
        cp -r "$EXTERNAL_CACHE/$repo" "$VENDOR_DIR/$repo"
    fi
done

echo
echo "Vendored to: $VENDOR_DIR"
echo "Run: loupe analyze $(pwd) --external-repo-root $VENDOR_DIR"
