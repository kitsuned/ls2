#!/bin/sh
set -eu

stage=$1
mode=$2
if [ "$#" -eq 2 ]; then
    if [ "$mode" = full ]; then
        echo 'Active TV hub baseline'
        if ! clock=$(timeout -k 2 8 /usr/bin/luna-send -t 1 luna://com.palm.systemservice/time/getSystemTime '{}' 2>&1); then
            printf '%s\n' "$clock" >&2
            exit 1
        fi
        printf '%s\n' "$clock"
        if ! printf '%s\n' "$clock" | grep -Eq '"returnValue"[[:space:]]*:[[:space:]]*true'; then
            echo 'The installed luna-send did not return a successful clock response.' >&2
            exit 1
        fi
        native_outcome=$(timeout -k 2 15 "$stage/native-fixture" active)
        rust_outcome=$(timeout -k 2 15 "$stage/tv_smoke" active)
        printf 'Native fixture %s\nRust fixture %s\n' "$native_outcome" "$rust_outcome"
        case "$native_outcome/$rust_outcome" in
            ACTIVE_OK/ACTIVE_OK|ACTIVE_DENIED/ACTIVE_DENIED) ;;
            *) echo 'Native and Rust fixtures must agree on the active hub outcome.' >&2; exit 1 ;;
        esac
    fi

    original_namespace=$(readlink /proc/self/ns/mnt)
    exec unshare --mount --propagation private /bin/sh "$stage/session.sh" "$stage" "$mode" "$original_namespace"
fi

if [ "$#" -ne 3 ] || [ "$3" = "$(readlink /proc/self/ns/mnt)" ]; then
    echo 'Isolated TV tests require a separate mount namespace.' >&2
    exit 1
fi

mount -n -t tmpfs -o mode=755 tmpfs /var/run/luna-service2
mount -n -t tmpfs -o mode=755 tmpfs /var/run/ls2
mount -n -t tmpfs -o mode=1777 tmpfs /dev/shm

policy=$stage/policy
mkdir -p "$policy/manifests" "$policy/volatile" "$policy/containers"

cat > "$stage/hub.conf" <<EOF
[General]
PidDirectory=/var/run/ls2
LogServiceStatus=false
ConnectTimeout=3000

[Watchdog]
Timeout=60
FailureMode=noop

[Dynamic Services]
LaunchTimeout=3000

[Security]
Enabled=true
HacksEnabled=false
MonitorExePath=/usr/sbin/ls-monitor
JsServiceExePath=js
AllowNullOutboundByDefault=false
ContainersDirectories=$policy/containers
ManifestsDirectories=$policy/manifests
ManifestsVolatileDirectories=$policy/volatile
DevmodeCertificate=$policy/devmode-certificate.json
DefaultDevmodeCertificate=/usr/share/luna-service2/devmode_certificate.json
AllowedGroups=$policy/allowed.json
ForcedTrust=$policy/forced.json

[Launcher]
Enabled=false
EOF

write_service() {
    exe=$1
    name=$2
    peer=$3
    cat > "$policy/$name.role.json" <<EOF
{
  "exeName": "$stage/$exe",
  "type": "regular",
  "trustLevel": "dev",
  "allowedNames": ["$name"],
  "permissions": [{"service": "$name", "inbound": ["$peer"], "outbound": ["$peer"]}]
}
EOF
    cat > "$policy/$name.service" <<EOF
[D-BUS Service]
Name=$name
Exec=$stage/$exe serve
Type=static
EOF
}

write_service tv_smoke org.ls2.test.rust org.ls2.test.native
write_service native-fixture org.ls2.test.native org.ls2.test.rust

cat > "$policy/api.json" <<'EOF'
{
  "org.ls2.test.calls": [
    "org.ls2.test.rust/test/echo",
    "org.ls2.test.native/test/echo",
    "org.ls2.test.rust/test/updates",
    "org.ls2.test.native/test/updates",
    "org.ls2.test.rust/com/palm/luna/private/cancel",
    "org.ls2.test.native/com/palm/luna/private/cancel",
    "com.webos.service.bus/signal/publish/test/changed",
    "com.webos.service.bus/signal/subscribe/test/changed"
  ],
  "org.ls2.test.secret": ["org.ls2.test.rust/test/secret", "org.ls2.test.native/test/secret"]
}
EOF

cat > "$policy/client.json" <<'EOF'
{
  "org.ls2.test.rust": ["org.ls2.test.calls"],
  "org.ls2.test.native": ["org.ls2.test.calls"]
}
EOF

cat > "$policy/groups.json" <<'EOF'
{
  "allowedNames": ["org.ls2.test.rust", "org.ls2.test.native"],
  "org.ls2.test.calls": ["dev"],
  "org.ls2.test.secret": ["dev"]
}
EOF

cat > "$policy/allowed.json" <<'EOF'
{"dev": ["org.ls2.test.calls", "org.ls2.test.secret"]}
EOF
printf '{}\n' > "$policy/forced.json"

cat > "$policy/manifests/org.ls2.test.json" <<EOF
{
  "id": "org.ls2.test",
  "version": "1.0.0",
  "roleFiles": ["$policy/org.ls2.test.rust.role.json", "$policy/org.ls2.test.native.role.json"],
  "serviceFiles": ["$policy/org.ls2.test.rust.service", "$policy/org.ls2.test.native.service"],
  "apiPermissionFiles": ["$policy/api.json"],
  "clientPermissionFiles": ["$policy/client.json"],
  "groupsFiles": ["$policy/groups.json"]
}
EOF

hub_pid=''
fixture_pid=''

stop() {
    pid=$1
    if [ -n "$pid" ]; then
        kill "$pid" 2>/dev/null || :
        retries=0
        while kill -0 "$pid" 2>/dev/null && [ "$retries" -lt 20 ]; do
            sleep 0.05
            retries=$((retries + 1))
        done
        kill -KILL "$pid" 2>/dev/null || :
        wait "$pid" 2>/dev/null || :
    fi
}

cleanup() {
    status=$?
    trap - EXIT
    stop "$fixture_pid"
    stop "$hub_pid"
    if [ "$status" -ne 0 ]; then
        for log in "$stage"/*.log; do
            if [ -f "$log" ]; then
                printf '\n%s\n' "${log##*/}" >&2
                tail -n 50 "$log" >&2
            fi
        done
    fi
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

wait_for() {
    watched_pid=$1
    shift
    retries=0
    until "$@"; do
        if [ "$retries" -ge 100 ] || ! kill -0 "$watched_pid" 2>/dev/null; then
            echo "Timed out waiting for $*" >&2
            return 1
        fi
        sleep 0.05
        retries=$((retries + 1))
    done
}

start() {
    fixture=$1
    mode=$2
    fixture_log=$stage/$fixture-$mode.log
    timeout -k 2 30 "$stage/$fixture" "$mode" > "$fixture_log" 2>&1 &
    fixture_pid=$!
    wait_for "$fixture_pid" grep -q '^READY$' "$fixture_log"
}

check_service() {
    server=$1
    client=$2
    start "$server" serve
    timeout -k 2 16 "$stage/$client" client
    wait_for "$fixture_pid" grep -q '^CANCELLED$' "$fixture_log"
    if grep -q '^SECRET_DISPATCHED$' "$fixture_log"; then
        echo 'The denied method ran.' >&2
        return 1
    fi
    stop "$fixture_pid"
    fixture_pid=''
    echo "$client -> $server passed calls, ordered updates, cancellation, and method ACL"
}

check_signal() {
    watcher=$1
    emitter=$2
    start "$watcher" watch
    timeout -k 2 8 "$stage/$emitter" emit
    # The watcher exits after one checked payload or its timeout.
    wait "$fixture_pid"
    fixture_pid=''
    echo "$emitter -> $watcher passed signal delivery"
}

/usr/sbin/ls-hubd --conf "$stage/hub.conf" > "$stage/hub.log" 2>&1 &
hub_pid=$!
wait_for "$hub_pid" test -S /var/run/luna-service2/com.palm.hub

check_service native-fixture tv_smoke
check_service tv_smoke native-fixture
check_signal tv_smoke native-fixture
check_signal native-fixture tv_smoke
