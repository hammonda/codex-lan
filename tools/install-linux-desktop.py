#!/usr/bin/env python3
"""Install the current user's server launcher; --pin adds it to GNOME favorites."""
import argparse
import ast
import os
from pathlib import Path
import shutil
import subprocess

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--pin', action='store_true')
parser.add_argument('--binary', type=Path)
args = parser.parse_args()
root = Path(__file__).resolve().parents[1]
binary = (args.binary or (root / 'codex-lan-server' if (root / 'codex-lan-server').exists() else root / 'target/release/codex-lan-server')).resolve()
if not binary.is_file():
    parser.error(f'Build or extract the server first: {binary}')
data = Path(os.environ.get('XDG_DATA_HOME', Path.home() / '.local/share'))
config = Path(os.environ.get('XDG_CONFIG_HOME', Path.home() / '.config')) / 'codex-lan/connection.json'
icon_theme = data / 'icons/hicolor'
icons = icon_theme / 'scalable/apps'
png_icon = icon_theme / '256x256/apps/codex-lan-server.png'
applications = data / 'applications'
icons.mkdir(parents=True, exist_ok=True)
png_icon.parent.mkdir(parents=True, exist_ok=True)
applications.mkdir(parents=True, exist_ok=True)
shutil.copyfile(root / 'assets/codex-lan-server.svg', icons / 'codex-lan-server.svg')
shutil.copyfile(root / 'assets/codex-lan-server.png', png_icon)
if shutil.which('gtk-update-icon-cache'):
    subprocess.run(['gtk-update-icon-cache', '--force', '--ignore-theme-index', str(icon_theme)], check=True)

def quote(value):
    text = str(value).replace('%', '%%')
    for character in ('\\', '"', '`', '$'):
        text = text.replace(character, '\\' + character)
    return '"' + text + '"'

desktop_id = 'codex-lan-server.desktop'
entry = applications / desktop_id
entry.write_text(
    '[Desktop Entry]\nType=Application\nName=Codex LAN Server\n'
    'Comment=Monitor Codex sessions and connect Claude Code\n'
    f'Exec={quote(binary)} --start --connection-file {quote(config)}\n'
    f'Icon={png_icon}\nTerminal=false\nCategories=Development;\n'
    'StartupWMClass=codex-lan-server\nStartupNotify=true\nSingleMainWindow=true\n', encoding='utf-8')
entry.chmod(0o644)
if shutil.which('update-desktop-database'):
    subprocess.run(['update-desktop-database', str(applications)], check=True)
if args.pin:
    favorites = subprocess.check_output(['gsettings', 'get', 'org.gnome.shell', 'favorite-apps'], text=True).strip().removeprefix('@as ')
    favorites = ast.literal_eval(favorites)
    if desktop_id not in favorites:
        favorites.append(desktop_id)
        subprocess.run(['gsettings', 'set', 'org.gnome.shell', 'favorite-apps', repr(favorites)], check=True)
print(f'Installed {entry}' + (' and pinned to GNOME favorites' if args.pin else ''))
