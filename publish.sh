#!/usr/bin/env bash

set -ex

git diff --exit-code

# Rename Cargo.toml or else cargo-package won't include it
mv sbp-settings-sys/src/libsettings/third_party/libsbp/Cargo.toml \
    sbp-settings-sys/src/libsettings/third_party/libsbp/_Cargo.toml

cargo release "$@"

mv sbp-settings-sys/src/libsettings/third_party/libsbp/_Cargo.toml \
    sbp-settings-sys/src/libsettings/third_party/libsbp/Cargo.toml
