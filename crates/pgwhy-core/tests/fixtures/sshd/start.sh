#!/bin/sh
# Boot an alpine container into a minimal sshd (throwaway test fixture).
set -e
apk add --no-cache openssh-server >/dev/null
adduser -D -s /bin/sh pgwhy
echo "pgwhy:$SSH_PASSWORD" | chpasswd >/dev/null
ssh-keygen -q -t ed25519 -N '' -f /etc/ssh/ssh_host_ed25519_key
exec /usr/sbin/sshd -D -e -f /fixture/sshd_config
