#!/usr/bin/env bash

set -ex

git diff --exit-code

# Rename Cargo.toml or else cargo-package won't include it
mv src/libsettings/third_party/libsbp/Cargo.toml \
    src/libsettings/third_party/libsbp/_Cargo.toml

if [[ -z "${EXECUTE}" ]]; then
    FLAGS="--dry-run"
else
    FLAGS=""
fi

cargo publish --allow-dirty $FLAGS

mv src/libsettings/third_party/libsbp/_Cargo.toml \
    src/libsettings/third_party/libsbp/Cargo.toml
