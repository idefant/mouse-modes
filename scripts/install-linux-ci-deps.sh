#!/usr/bin/env bash
# Используется только на одноразовом Ubuntu-раннере GitHub Actions.
set -euo pipefail

packages=(libwebkit2gtk-4.1-dev build-essential curl wget file libxdo-dev libssl-dev librsvg2-dev libayatana-appindicator3-dev libsystemd-dev rpm libarchive-tools)
apt_options=(-o Acquire::Retries=1 -o Acquire::http::Timeout=20 -o Acquire::https::Timeout=20)
source_backup=$(mktemp -d)
trap 'rm -rf "$source_backup"' EXIT

# Сохраняем исходные зеркала как резерв, не меняя репозитории и проверку подписей.
mapfile -t source_files < <(find /etc/apt -maxdepth 2 -type f \( -name '*.sources' -o -name '*.list' \))
for source_file in "${source_files[@]}"; do
  cp --parents "$source_file" "$source_backup"
done

sudo python3 - "${source_files[@]}" <<'PY'
import re
import sys
from pathlib import Path

for filename in sys.argv[1:]:
    path = Path(filename)
    text = path.read_text()
    # Образы раннеров могут хранить Azure URL в mirrorlist вместо самого sources.
    text = text.replace('mirror+file:/etc/apt/apt-mirrors-security.txt', 'https://security.ubuntu.com/ubuntu/')
    text = text.replace('mirror+file:/etc/apt/apt-mirrors.txt', 'https://archive.ubuntu.com/ubuntu/')
    text = re.sub(r'https?://(?:azure\.)?archive\.ubuntu\.com/ubuntu/?', 'https://archive.ubuntu.com/ubuntu/', text)
    text = re.sub(r'https?://security\.ubuntu\.com/ubuntu/?', 'https://security.ubuntu.com/ubuntu/', text)
    path.write_text(text)
PY

download_packages() {
  # Ограничение относится только к сети: dpkg никогда не прерывается таймером.
  timeout --kill-after=10s 180s sudo apt-get "${apt_options[@]}" update &&
    timeout --kill-after=10s 180s sudo apt-get "${apt_options[@]}" install --download-only -y "${packages[@]}"
}

if ! download_packages; then
  echo 'Official Ubuntu mirror failed or exceeded its download budget; retrying original runner mirrors.'
  for source_file in "${source_files[@]}"; do
    sudo cp "$source_backup$source_file" "$source_file"
  done
  download_packages
fi

# Все архивы уже проверены и скачаны. Ошибка установки завершает job.
sudo env DEBIAN_FRONTEND=noninteractive apt-get "${apt_options[@]}" install --no-download -y "${packages[@]}"
