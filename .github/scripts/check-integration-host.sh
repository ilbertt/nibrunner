#!/usr/bin/env bash
set -euo pipefail

uname -a
cat /etc/os-release
lscpu
ls -l /dev/net/tun /dev/kvm
test -c /dev/net/tun
test -c /dev/kvm
cat /sys/fs/cgroup/cgroup.controllers
