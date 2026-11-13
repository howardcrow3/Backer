#!/usr/bin/env bash
# Start throwaway SFTP, SMB and NFS servers in Docker for the connector integration tests, and print
# the environment variables the tests read. Used by CI; run it locally the same way:
#
#   eval "$(scripts/nas-test-servers.sh up)"
#   cargo test -p backer-connector --test protocols
#   scripts/nas-test-servers.sh down
#
# NFS needs a Linux host whose kernel has nfsd (GitHub's ubuntu runners do); it is skipped elsewhere.
set -euo pipefail
PASS="backer-test-pw"
SFTP_PORT=2222
SMB_PORT=1445

up() {
  docker rm -f backer-sftp backer-smb backer-nfs >/dev/null 2>&1 || true
  # SFTP: user "backer", writable folder /upload (OpenSSH, like a Synology).
  docker run -d --name backer-sftp -p 127.0.0.1:$SFTP_PORT:22 atmoz/sftp:alpine "backer:$PASS:1001:100:upload" >/dev/null
  # SMB 3 (Samba 4.2x with the macOS "fruit" module, as on a Synology): user "backer", share "backups".
  local cfg="${RUNNER_TEMP:-/tmp}/backer-smb"
  mkdir -p "$cfg/data"
  chmod 777 "$cfg/data"
  cat > "$cfg/config.yml" <<YML
auth:
  - user: backer
    group: backer
    uid: 1000
    gid: 1000
    password: $PASS
global:
  - "force user = backer"
  - "force group = backer"
share:
  - name: backups
    path: /samba/backups
    browsable: yes
    readonly: no
    guestok: no
    validusers: backer
    writelist: backer
YML
  docker run -d --name backer-smb -p 127.0.0.1:$SMB_PORT:445 -v "$cfg:/data" -v "$cfg/data:/samba/backups" crazymax/samba:4.23.8 >/dev/null
  if [ "$(uname -s)" = "Linux" ] && { lsmod 2>/dev/null | grep -q '^nfsd' || sudo -n modprobe nfsd 2>/dev/null; }; then
    mkdir -p "${RUNNER_TEMP:-/tmp}/backer-nfs"
    chmod 777 "${RUNNER_TEMP:-/tmp}/backer-nfs"
    docker run -d --name backer-nfs --privileged -p 127.0.0.1:2049:2049 -p 127.0.0.1:111:111 -p 127.0.0.1:32767:32767 -p 127.0.0.1:32765:32765 \
      -v "${RUNNER_TEMP:-/tmp}/backer-nfs:/export" \
      -e NFS_EXPORT_0='/export *(rw,sync,no_subtree_check,insecure,no_root_squash,fsid=0)' \
      -e NFS_PORT_MOUNTD=32767 -e NFS_PORT_STATD_IN=32765 -e NFS_DISABLE_VERSION_3=  \
      erichough/nfs-server:2.2.1 >/dev/null
    NFS=1
  fi
  wait_port $SFTP_PORT; wait_port $SMB_PORT
  [ -n "${NFS:-}" ] && wait_port 2049
  # Samba and sshd accept connections a moment before they are ready to log in.
  sleep 3
  echo "export BACKER_TEST_SFTP='sftp://backer@127.0.0.1:$SFTP_PORT/upload'"
  echo "export BACKER_TEST_SFTP_PASSWORD='$PASS'"
  echo "export BACKER_TEST_SMB='smb://backer@127.0.0.1:$SMB_PORT/backups'"
  echo "export BACKER_TEST_SMB_PASSWORD='$PASS'"
  [ -n "${NFS:-}" ] && echo "export BACKER_TEST_NFS='nfs://127.0.0.1/export?uid=0&gid=0'"
  return 0
}

wait_port() {
  for _ in $(seq 1 60); do
    (exec 3<>/dev/tcp/127.0.0.1/"$1") 2>/dev/null && return 0
    sleep 1
  done
  echo "port $1 never opened" >&2; docker logs backer-sftp >&2 || true; docker logs backer-smb >&2 || true; return 1
}

down() { docker rm -f backer-sftp backer-smb backer-nfs >/dev/null 2>&1 || true; }

"${1:-up}"
