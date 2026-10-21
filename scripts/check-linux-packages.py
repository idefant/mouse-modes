#!/usr/bin/env python3
"""Validate the actual Linux archives, including permissions and lifecycle hooks."""
import io
import pathlib
import subprocess
import sys
import tarfile
import tempfile

required = {
    'usr/bin/mouse-modes',
    'usr/lib/systemd/system/mouse-modes-input.socket',
    'usr/lib/systemd/system/mouse-modes-input@.service',
    'usr/lib/sysusers.d/mouse-modes.conf',
    'usr/lib/modules-load.d/mouse-modes.conf',
    'usr/lib/udev/rules.d/70-mouse-modes.rules',
}


def run(*args):
    return subprocess.check_output(args)


def validate_payload(folder):
    for path in required:
        assert (folder / path).is_file(), path
    unit = (folder / 'usr/lib/systemd/system/mouse-modes-input@.service').read_text()
    for line in ['User=mouse-modes', 'Group=mouse-modes', 'SupplementaryGroups=input',
                 'ExecStart=/usr/bin/mouse-modes --input-service',
                 'NoNewPrivileges=yes', 'CapabilityBoundingSet=', 'ProtectHome=yes']:
        assert line in unit.splitlines(), line
    assert (folder / 'usr/lib/modules-load.d/mouse-modes.conf').read_text().strip() == 'uinput'
    assert 'u:mouse-modes:rw /dev/uinput' in (folder / 'usr/lib/udev/rules.d/70-mouse-modes.rules').read_text()
    assert not list(folder.glob('**/polkit-1/**/*.rules')), 'Package must not grant passwordless root'
    assert not list(folder.glob('**/polkit-1/**/*.policy')), 'Package must not need a per-user polkit workaround'


packages = [p for p in pathlib.Path(sys.argv[1]).rglob('*') if p.suffix in {'.deb', '.rpm'} or p.name.endswith('.pkg.tar.zst')]
assert packages, 'No Linux installers found'
for package in packages:
    with tempfile.TemporaryDirectory(prefix='mouse-modes-package-') as temporary:
        folder = pathlib.Path(temporary)
        if package.suffix == '.deb':
            archive = run('dpkg-deb', '--fsys-tarfile', str(package))
            with tarfile.open(fileobj=io.BytesIO(archive)) as tar:
                entries = {p.name.removeprefix('./'): p for p in tar.getmembers()}
                for path in required:
                    entry = entries[path]
                    assert (entry.uid, entry.gid) == (0, 0), path
                    assert not entry.mode & 0o6022, path
                assert entries['usr/bin/mouse-modes'].mode & 0o111
            dependencies = run('dpkg-deb', '--field', str(package), 'Depends').decode()
            assert all(name in dependencies for name in ['systemd', 'acl', 'kmod', 'udev', 'libsystemd0'])
            run('dpkg-deb', '--extract', str(package), str(folder))
            control = folder / 'control'
            run('dpkg-deb', '--control', str(package), str(control))
            hooks = {name: (control / name).read_text() for name in ['postinst', 'prerm', 'postrm']}
        elif package.suffix == '.rpm':
            metadata = run('rpm', '-qp', '--qf', '[%{FILENAMES}\t%{FILEUSERNAME}\t%{FILEGROUPNAME}\t%{FILEMODES:octal}\n]', str(package)).decode()
            entries = {line.split('\t')[0].lstrip('/'): line.split('\t')[1:] for line in metadata.splitlines()}
            for path in required:
                user, group, mode = entries[path]
                assert (user, group) == ('root', 'root'), path
                assert not int(mode, 8) & 0o6022, path
            run('rpm', '--checksig', str(package))
            run('bsdtar', '-xf', str(package), '-C', str(folder))
            scripts = run('rpm', '-qp', '--scripts', str(package)).decode()
            hooks = {'postinst': scripts, 'prerm': scripts, 'postrm': scripts}
        else:
            archive = run('bsdtar', '-cf', '-', '--format=pax', '@' + str(package))
            with tarfile.open(fileobj=io.BytesIO(archive)) as tar:
                entries = {p.name.removeprefix('./'): p for p in tar.getmembers()}
                for path in required:
                    entry = entries[path]
                    assert (entry.uid, entry.gid) == (0, 0), path
                    assert not entry.mode & 0o6022, path
            run('bsdtar', '-xf', str(package), '-C', str(folder))
            scripts = (folder / '.INSTALL').read_text()
            hooks = {'postinst': scripts, 'prerm': scripts, 'postrm': scripts}
        validate_payload(folder)
        assert 'systemd-sysusers' in hooks['postinst']
        assert 'systemctl enable mouse-modes-input.socket' in hooks['postinst']
        assert 'systemctl disable mouse-modes-input.socket' in hooks['prerm']
        assert 'setfacl -x u:mouse-modes' in hooks['postrm']
        print(f'{package.name}: payload, ownership, service isolation, install/remove hooks OK')
