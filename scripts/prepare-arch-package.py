#!/usr/bin/env python3
"""Prepare a native Arch package from the same Linux release payload."""
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys

root = Path(__file__).resolve().parents[1]
version = json.loads((root / 'package.json').read_text())['version']
assert re.fullmatch(r'\d+\.\d+\.\d+', version), 'Arch release packages require a stable version'
package, output = map(lambda value: Path(value).resolve(), sys.argv[1:])
output.mkdir(parents=True, exist_ok=True)
archive = subprocess.check_output(['bsdtar', '-xOf', str(package), 'data.tar.gz'])
(output / 'payload.tar.gz').write_bytes(archive)
checksum = hashlib.sha256(archive).hexdigest()
(output / 'PKGBUILD').write_text(f'''pkgname=mouse-modes
pkgver={version}
pkgrel=1
pkgdesc='Hold a hotkey to change mouse movement into precision, speed or scrolling'
arch=('x86_64')
url='https://github.com/idefant/mouse-modes'
license=('custom')
depends=('webkit2gtk-4.1' 'libappindicator-gtk3' 'systemd' 'acl' 'kmod')
options=('!debug')
install=mouse-modes.install
source=('payload.tar.gz')
sha256sums=('{checksum}')
package() {{
  cp -a "$srcdir/usr" "$pkgdir/"
}}
''')
functions = []
for name, file in [('post_install', 'post-install.sh'), ('pre_remove', 'pre-remove.sh'), ('post_remove', 'post-remove.sh')]:
    body = (root / 'src-tauri/linux' / file).read_text().splitlines()[2:]
    functions.append(name + '() {\n' + '\n'.join('  ' + line for line in body) + '\n}\n')
functions.append('post_upgrade() { post_install; }\n')
(output / 'mouse-modes.install').write_text('\n'.join(functions))
print(f'Arch package sources ready in {output}')
