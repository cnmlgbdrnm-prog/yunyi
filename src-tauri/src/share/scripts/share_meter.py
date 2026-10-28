#!/usr/bin/env python3
"""Root-owned IP meter. Its counters must never be synthesized from payloads."""
import argparse
import ctypes
import hashlib
import json
import os
from pathlib import Path
import platform
import signal
import socket
import struct
import subprocess
import time

ROOT = Path('/srv/vpnshare')
PINS = Path('/sys/fs/bpf/vpnshare')
STATE_ROOT = Path('/var/lib/vpnshare-meters')


def run(args, timeout=30):
    result = subprocess.run(args, capture_output=True, text=True, timeout=timeout)
    if result.returncode:
        raise RuntimeError(f'{args[0]} failed: {result.stderr[-1500:]}')
    return result.stdout.strip()


class Map:
    """Use BPF_F_LOCK for consistent snapshots of the spin-locked map value."""
    def __init__(self, path, size):
        numbers = {'x86_64': 321, 'aarch64': 280}
        self.number = numbers.get(platform.machine())
        if self.number is None:
            raise RuntimeError('Unsupported BPF syscall architecture')
        self.libc = ctypes.CDLL(None, use_errno=True)
        self.size = size
        name = ctypes.create_string_buffer(os.fsencode(path))
        attr = ctypes.create_string_buffer(struct.pack('=QII', ctypes.addressof(name), 0, 0))
        self.fd = self.call(7, attr, 16)

    def call(self, command, attr, size):
        result = self.libc.syscall(self.number, command, ctypes.byref(attr), size)
        if result < 0:
            code = ctypes.get_errno()
            raise OSError(code, os.strerror(code))
        return result

    def access(self, data=None):
        key = ctypes.c_uint32(0)
        value = ctypes.create_string_buffer(data, self.size) if data is not None else ctypes.create_string_buffer(self.size)
        attr = ctypes.create_string_buffer(struct.pack('=IIQQQ', self.fd, 0, ctypes.addressof(key), ctypes.addressof(value), 4))
        self.call(2 if data is not None else 1, attr, 32)
        return value.raw

    def close(self):
        os.close(self.fd)


def prepare(source):
    source = Path(source)
    digest = hashlib.sha256(source.read_bytes()).hexdigest()
    obj = source.with_name('share_meter-' + digest[:16] + '.o')
    if not obj.exists():
        machine = platform.machine()
        include = '/usr/include/' + {'x86_64': 'x86_64-linux-gnu', 'aarch64': 'aarch64-linux-gnu'}.get(machine, machine)
        temp = obj.with_suffix('.tmp.o')
        run(['clang', '-O2', '-g', '-target', 'bpf', '-I'+include, '-c', str(source), '-o', str(temp)], 60)
        os.chmod(temp, 0o600)
        os.replace(temp, obj)
    return obj


def load_maps(identifier, source, pin_root=PINS):
    directory = pin_root / identifier
    fresh = not (directory / 'maps/traffic').exists()
    if fresh:
        (directory / 'maps').mkdir(parents=True, mode=0o700, exist_ok=True)
        run(['bpftool', 'prog', 'loadall', str(prepare(source)), str(directory/'programs'), 'pinmaps', str(directory/'maps')])
    for name in ('programs/share_ingress', 'programs/share_egress', 'maps/config', 'maps/traffic'):
        if not (directory/name).exists():
            raise RuntimeError('Incomplete pinned meter; refusing to reset counters')
        os.chmod(directory/name, 0o600)
    return directory, fresh, Map(directory/'maps/config', 32), Map(directory/'maps/traffic', 32)


def set_config(config, quota, history, ifindex, frozen=False):
    config.access(struct.pack('=IIQQII', 0, 0, quota, history, ifindex, int(frozen)))


def set_counters(traffic, rx, tx, denied=0):
    traffic.access(struct.pack('=IIQQQ', 0, 0, rx, tx, denied))


def get_counters(traffic):
    _, _, rx, tx, _ = struct.unpack('=IIQQQ', traffic.access())
    return rx, tx


def public_interface():
    routes = json.loads(run(['ip', '-j', 'route', 'show', 'default']))
    devices = {route['dev'] for route in routes if route.get('dev')}
    if len(devices) != 1:
        raise RuntimeError('Accurate IP accounting requires one explicit public interface')
    device = devices.pop()
    return device, socket.if_nametoindex(device)


def atomic_json(path, value):
    temporary = path.with_name(path.name + '.tmp')
    with open(temporary, 'w', encoding='utf-8') as output:
        json.dump(value, output, ensure_ascii=False)
        output.flush()
        os.fsync(output.fileno())
    os.chmod(temporary, 0o600)
    # Existing share services need to read the public status snapshot.
    if path.name == 'meter-status.json':
        import pwd
        account = pwd.getpwnam('vpnshare')
        os.chown(temporary, account.pw_uid, account.pw_gid)
    os.replace(temporary, path)
    fd = os.open(path.parent, os.O_DIRECTORY)
    try: os.fsync(fd)
    finally: os.close(fd)


def timestamp():
    return time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())


def notify(message):
    address = os.environ.get('NOTIFY_SOCKET')
    if not address:
        return
    if address.startswith('@'):
        address = '\0' + address[1:]
    with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as client:
        client.connect(address)
        client.sendall(message.encode())


def valid_id(identifier):
    if len(identifier) != 16 or any(c not in '0123456789abcdef' for c in identifier):
        raise ValueError('Invalid share id')
    return identifier


def boot_id():
    return Path('/proc/sys/kernel/random/boot_id').read_text().strip()


def read_manifest(identifier):
    manifest = json.loads((ROOT/identifier/'share.json').read_text())
    if manifest['id'] != identifier:
        raise ValueError('Share identity mismatch')
    quota = manifest.get('quota_total_bytes')
    if quota is None or not isinstance(quota, int) or quota < 0:
        raise ValueError('Total quota is not configured')
    return manifest


def historical_payload(directory):
    usage = json.loads((directory/'usage.json').read_text())
    floor = directory/'usage.json.history'
    if floor.exists():
        saved = json.loads(floor.read_text())
        for key in ('used_bytes', 'used_up_bytes', 'used_down_bytes'):
            usage[key] = max(int(usage.get(key, 0)), int(saved.get(key, 0)))
    if 'used_up_bytes' in usage and 'used_down_bytes' in usage:
        up, down = int(usage['used_up_bytes']), int(usage['used_down_bytes'])
    else:
        up, down = 0, int(usage['used_bytes'])
    if up < 0 or down < 0:
        raise ValueError('Invalid historical usage')
    return up, down


def initialize_state(directory, boot):
    up, down = historical_payload(directory)
    return {'version': 1, 'started_at': timestamp(), 'boot_id': boot,
            'history_basis': 'user_confirmed_2x_gateway',
            'history_up_bytes': up, 'history_down_bytes': down,
            'history_charge_bytes': 2*(up+down),
            'public_rx_bytes': 0, 'public_tx_bytes': 0, 'denied_quota': 0,
            'clean_shutdown': False}


def serve(identifier):
    directory = ROOT/identifier
    STATE_ROOT.mkdir(mode=0o700, parents=True, exist_ok=True)
    state_path = STATE_ROOT/(identifier+'.json')
    status_path = directory/'meter-status.json'
    boot = boot_id()
    manifest = read_manifest(identifier)
    state_existed = state_path.exists()
    state = json.loads(state_path.read_text()) if state_existed else initialize_state(directory, boot)
    if state.get('suspended_payload') is not None:
        # Only a failed first enablement can temporarily restore a legacy
        # configuration. Fold that unmetered gap once, retaining the ledger.
        up, down = historical_payload(directory)
        previous_up, previous_down = state['suspended_payload']
        if up < previous_up or down < previous_down:
            raise RuntimeError('Historical payload moved backwards during rollback')
        state['history_charge_bytes'] += 2*(up-previous_up+down-previous_down)
        state['history_up_bytes'] += up-previous_up
        state['history_down_bytes'] += down-previous_down
        del state['suspended_payload']
    source = ROOT/'share_meter.bpf.c'
    digest = hashlib.sha256(source.read_bytes()).hexdigest()
    if state.get('program_sha256', digest) != digest:
        raise RuntimeError('Meter version changed; a counter-preserving upgrade is required')
    pins, fresh, config, traffic = load_maps(identifier, source)
    device, ifindex = public_interface()
    history = int(state['history_charge_bytes'])
    quota = manifest['quota_total_bytes']
    stopping = False
    def stop(*_):
        nonlocal stopping
        stopping = True
    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    try:
        set_config(config, quota, history, ifindex, frozen=True)
        if not state_existed and not fresh:
            raise RuntimeError('Historical ledger missing for existing kernel counters; refusing to fold history twice')
        if fresh:
            if state_path.exists() and not state.get('clean_shutdown'):
                raise RuntimeError('Kernel counters disappeared after an unclean stop; historical tail cannot be reconstructed accurately')
            set_counters(traffic, int(state['public_rx_bytes']), int(state['public_tx_bytes']), int(state.get('denied_quota', 0)))
        elif state.get('boot_id') != boot:
            raise RuntimeError('Unexpected meter boot identity')
        state.update(program_sha256=digest, boot_id=boot, clean_shutdown=False, interface=device)
        atomic_json(state_path, state)
        set_config(config, quota, history, ifindex)
        ready = False
        while not stopping:
            manifest = read_manifest(identifier)
            quota = manifest['quota_total_bytes']
            if socket.if_nametoindex(device) != ifindex:
                raise RuntimeError('Public interface changed; refusing an unmetered route')
            set_config(config, quota, history, ifindex)
            _, _, rx, tx, denied = struct.unpack('=IIQQQ', traffic.access())
            if rx < int(state['public_rx_bytes']) or tx < int(state['public_tx_bytes']):
                raise RuntimeError('Meter counters moved backwards')
            state.update(public_rx_bytes=rx, public_tx_bytes=tx, denied_quota=denied, updated_at=timestamp())
            atomic_json(state_path, state)
            total = history + rx + tx
            atomic_json(status_path, {
                'state': 'blocked_quota' if quota and (total >= quota or denied == quota) else 'ready',
                'total_used_bytes': total, 'quota_total_bytes': quota,
                'history_charge_bytes': history, 'history_basis': state['history_basis'],
                'public_rx_bytes': rx, 'public_tx_bytes': tx,
                'started_at': state['started_at'], 'updated_at': state['updated_at'],
                'basis': 'public_interface_cgroup_ip_bytes', 'interface': device,
            })
            if not ready:
                notify('READY=1')
                ready = True
            notify('WATCHDOG=1')
            time.sleep(0.5)
        # BindsTo/After makes the data units stop before their meter. Persist
        # the final kernel snapshot; restart in the same boot reuses the maps.
        set_config(config, quota, history, ifindex, frozen=True)
        _, _, rx, tx, denied = struct.unpack('=IIQQQ', traffic.access())
        state.update(public_rx_bytes=rx, public_tx_bytes=tx, denied_quota=denied,
                     clean_shutdown=True, updated_at=timestamp())
        atomic_json(state_path, state)
    except BaseException as error:
        set_config(config, quota, history, ifindex, frozen=True)
        atomic_json(status_path, {'state':'error','detail':str(error),'started_at':state.get('started_at'),
                                 'updated_at':timestamp(),'history_charge_bytes':history})
        raise
    finally:
        config.close(); traffic.close()


def sync(identifier):
    manifest = read_manifest(identifier)
    state = json.loads((STATE_ROOT/(identifier+'.json')).read_text())
    config = Map(PINS/identifier/'maps/config', 32)
    try:
        _, _, _, _, ifindex, frozen = struct.unpack('=IIQQII', config.access())
        if frozen:
            raise RuntimeError('Meter is frozen; refusing to clear its error')
        set_config(config, manifest['quota_total_bytes'], state['history_charge_bytes'], ifindex)
    finally: config.close()


def remove(identifier):
    import shutil
    unit = 'vpnshare-'+identifier
    subprocess.run(['systemctl','disable','--now',unit+'-meter.service'],capture_output=True,timeout=30)
    # All referencing data services are BindsTo the meter and must be stopped.
    for suffix in ('', '-gateway', '-xray', '-singbox', '-ss'):
        result = subprocess.run(['systemctl','is-active',unit+suffix],capture_output=True,text=True)
        if result.stdout.strip() in ('active','activating','deactivating'):
            raise RuntimeError('Share services are not stopped; cannot remove its meter')
    subprocess.run(['systemctl','stop',unit+'.slice'],capture_output=True,timeout=30)
    pin = PINS/identifier
    if pin.exists(): shutil.rmtree(pin)
    (STATE_ROOT/(identifier+'.json')).unlink(missing_ok=True)
    for suffix in ('', '-gateway', '-xray', '-singbox', '-ss'):
        (Path('/etc/systemd/system')/(unit+suffix+'.service.d')/'90-share-meter.conf').unlink(missing_ok=True)
    for path in (Path('/etc/systemd/system')/(unit+'-meter.service'),
                 Path('/etc/systemd/system')/(unit+'.slice')):
        path.unlink(missing_ok=True)
    run(['systemctl','daemon-reload'])


def reconcile(identifier):
    manifest = json.loads((ROOT/identifier/'share.json').read_text())
    unit = 'vpnshare-'+identifier
    if manifest.get('quota_total_bytes') is not None:
        run(['systemctl','restart',unit+'-meter.service'])
        return
    # Failed enablement may roll back config, but its historical ledger and
    # pinned counters are never erased while this link still exists.
    subprocess.run(['systemctl','disable','--now',unit+'-meter.service'],capture_output=True,timeout=30)
    state_path = STATE_ROOT/(identifier+'.json')
    if state_path.exists():
        state = json.loads(state_path.read_text())
        if not state.get('clean_shutdown'):
            raise RuntimeError('Meter did not stop cleanly; refusing unmetered rollback')
        state['suspended_payload'] = historical_payload(ROOT/identifier)
        atomic_json(state_path, state)
    subprocess.run(['systemctl','stop',unit+'.slice'],capture_output=True,timeout=30)
    for suffix in ('','-gateway','-xray','-singbox','-ss'):
        (Path('/etc/systemd/system')/(unit+suffix+'.service.d')/'90-share-meter.conf').unlink(missing_ok=True)
    (Path('/etc/systemd/system')/(unit+'.slice')).unlink(missing_ok=True)
    run(['systemctl','daemon-reload'])


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('action', choices=('prepare','serve','sync','remove','reconcile'))
    parser.add_argument('id', nargs='?')
    args = parser.parse_args()
    if args.action == 'prepare':
        print(prepare(ROOT/'share_meter.bpf.c'))
    else:
        identifier = valid_id(args.id or '')
        {'serve':serve,'sync':sync,'remove':remove,'reconcile':reconcile}[args.action](identifier)
