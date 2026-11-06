#!/usr/bin/env python3
import fcntl
import os

with open("/dev/kvm", "r+b", buffering=0) as kvm:
    api_version = fcntl.ioctl(kvm, 0xAE00, 0)
    if api_version != 12:
        raise RuntimeError(f"unsupported KVM API version: {api_version}")
    vm = fcntl.ioctl(kvm, 0xAE01, 0)
    os.close(vm)
