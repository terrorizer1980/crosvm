#!/usr/bin/env bash
# Copyright 2022 The ChromiumOS Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Ensure there's only 1 instance of setup-user.sh running
flock /tmp/entrypoint_lock /tools/setup-user.sh

# Give KVM device correct permission
chmod 666 /dev/kvm

# Run provided command or interactive shell
if [[ $# -eq 0 ]]; then
    sudo -u crosvmdev /bin/bash -l
else
    sudo -u crosvmdev /bin/bash -l -c "$*"
fi
