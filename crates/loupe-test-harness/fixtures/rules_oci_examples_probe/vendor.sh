#!/usr/bin/env bash
# Vendor the probe's transitive bzlmod deps into a directory Loupe can read.
#
# Loupe needs:
#   - every transitively-loaded `external/<canonical>/` repo on disk
#   - a `bazel_tools/` directory (Bazel's own embedded tools tree)
#   - the `bazel_features_globals` and `bazel_features_version`
#     sub-repos, which `bazel vendor //...` skips when the targets in
#     `//` don't directly load them but the lockfile-resolution path
#     in Loupe needs them.
#
# Output: $VENDOR_DIR (default /tmp/loupe-probe-vendor) populated with the
# above. Re-runnable.

set -euo pipefail

cd "$(dirname "$0")"

VENDOR_DIR="${VENDOR_DIR:-/tmp/loupe-probe-vendor}"
BAZEL_JVM_TRUSTSTORE="${BAZEL_JVM_TRUSTSTORE:-/etc/ssl/certs/java/cacerts}"
BAZEL_JVM_ARGS="--host_jvm_args=-Djavax.net.ssl.trustStore=${BAZEL_JVM_TRUSTSTORE}"

rm -rf "$VENDOR_DIR" MODULE.bazel.lock vendor

bazel "$BAZEL_JVM_ARGS" vendor --vendor_dir=vendor //...
mv vendor "$VENDOR_DIR"

# Bazel's `bazel_tools` lives in the install dir, not the external cache.
BAZEL_INSTALL=$(bazel "$BAZEL_JVM_ARGS" info install_base)
ln -sfn "$BAZEL_INSTALL/embedded_tools" "$VENDOR_DIR/bazel_tools"

# bazel vendor //... only fetches what targets in // transitively load.
# bazel_features's `version_extension` outputs (`bazel_features_globals`,
# `bazel_features_version`) get loaded inside bazel_features's own bzls,
# which Loupe's MODULE.bazel walk reaches but `bazel vendor` doesn't.
# `local_config_platform` is a host-platform repo Bazel auto-synthesises
# at the start of each build; it's never user-vendored. aspect_bazel_lib's
# toolchain BUILDs `load("@local_config_platform//:constraints.bzl",
# "HOST_CONSTRAINTS")`, so Loupe needs it on disk too.
# Pull all of these from the external cache.
EXTERNAL_CACHE=$(bazel "$BAZEL_JVM_ARGS" info output_base)/external
for repo in bazel_features~~version_extension~bazel_features_globals \
            bazel_features~~version_extension~bazel_features_version \
            local_config_platform; do
    if [[ -d "$EXTERNAL_CACHE/$repo" && ! -d "$VENDOR_DIR/$repo" ]]; then
        cp -r "$EXTERNAL_CACHE/$repo" "$VENDOR_DIR/$repo"
    fi
done

echo
echo "Vendored to: $VENDOR_DIR"
echo "Run: loupe analyze $(pwd) --external-repo-root $VENDOR_DIR --skip-kind oci_image_index"
