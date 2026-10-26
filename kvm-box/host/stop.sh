#!/bin/sh
set -eu
systemctl --user stop kvm-box-connector-dev.service kvm-box-broker-dev.service
