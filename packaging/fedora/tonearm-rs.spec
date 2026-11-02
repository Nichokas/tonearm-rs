# tonearm-rs.spec — Fedora/COPR packaging for Tonearm (Rust port),
# https://github.com/Nichokas/tonearm-rs
#
# Sources:
#   Source0: GitHub tag tarball of this repo (v%%{version}).
#   Source1: `cargo vendor` archive for the same tag, produced by
#            packaging/fedora/make-vendor.sh and attached to GitHub release
#            v%%{version}. COPR downloads both while making the SRPM; the RPM
#            build itself runs fully offline (cargo --offline --frozen).
#
# The package is named tonearm-rs so it can live next to nichokas/tonearm (Go);
# both ship /usr/bin/tonearm and the dev.dergs.Tonearm files, so they conflict.

%global appid dev.dergs.Tonearm

Name:           tonearm-rs
Version:        1.1.0
Release:        1%{?dist}
Summary:        Native GTK4/libadwaita TIDAL client (Rust port)

License:        GPL-3.0-or-later
URL:            https://github.com/Nichokas/tonearm-rs
Source0:        %{url}/archive/v%{version}/%{name}-%{version}.tar.gz
Source1:        %{url}/releases/download/v%{version}/%{name}-%{version}-vendor.tar.xz

ExclusiveArch:  x86_64 aarch64

BuildRequires:  cargo >= 1.85
BuildRequires:  rust >= 1.85
BuildRequires:  gcc
BuildRequires:  pkgconfig(gtk4) >= 4.20
BuildRequires:  pkgconfig(libadwaita-1) >= 1.8
BuildRequires:  pkgconfig(gstreamer-1.0)
BuildRequires:  pkgconfig(gstreamer-audio-1.0)
BuildRequires:  pkgconfig(gstreamer-base-1.0)
BuildRequires:  pkgconfig(libsecret-1)
BuildRequires:  pkgconfig(openssl)
BuildRequires:  pkgconfig(dbus-1)
BuildRequires:  glib2-devel
BuildRequires:  desktop-file-utils
BuildRequires:  libappstream-glib

Requires:       hicolor-icon-theme
# Playback (HTTPS streaming, decoding, DASH/HLS)
Requires:       gstreamer1-plugins-base%{?_isa}
Requires:       gstreamer1-plugins-good%{?_isa}
Requires:       gstreamer1-plugins-bad-free%{?_isa}
Requires:       glib-networking%{?_isa}
Recommends:     (gnome-keyring or kwallet or keepassxc)
Suggests:       gstreamer1-libav

Conflicts:      tonearm
Provides:       tonearm-app = %{version}-%{release}

%description
Tonearm is a native GTK4/libadwaita client for the TIDAL music streaming
service. This is the Rust port (gtk4-rs, libadwaita-rs, gstreamer-rs) of the
original Go application, with the same interface.

%prep
%autosetup -n %{name}-%{version}
tar -xf %{SOURCE1}
mkdir -p .cargo
cat > .cargo/config.toml <<'CFG'
[source.crates-io]
replace-with = "vendored-sources"

[source.vendored-sources]
directory = "vendor"
CFG

%build
export TONEARM_VERSION=%{version}
export TONEARM_COMMIT=v%{version}
export RUSTFLAGS="%{?build_rustflags}"
cargo build --release --offline --frozen

%install
install -Dm0755 target/release/tonearm %{buildroot}%{_bindir}/tonearm
install -Dm0644 internal/icons/hicolor/scalable/apps/%{appid}.svg \
    %{buildroot}%{_datadir}/icons/hicolor/scalable/apps/%{appid}.svg
install -Dm0644 internal/icons/hicolor/symbolic/apps/%{appid}-symbolic.svg \
    %{buildroot}%{_datadir}/icons/hicolor/symbolic/apps/%{appid}-symbolic.svg
for size in 16 24 32 48 64 128 256 512; do
    install -Dm0644 internal/icons/hicolor/${size}x${size}/apps/%{appid}.png \
        %{buildroot}%{_datadir}/icons/hicolor/${size}x${size}/apps/%{appid}.png
done
install -Dm0644 build/%{appid}.desktop %{buildroot}%{_datadir}/applications/%{appid}.desktop
install -Dm0644 build/%{appid}.metainfo.xml %{buildroot}%{_metainfodir}/%{appid}.metainfo.xml
install -Dm0644 internal/settings/%{appid}.gschema.xml \
    %{buildroot}%{_datadir}/glib-2.0/schemas/%{appid}.gschema.xml

%check
desktop-file-validate %{buildroot}%{_datadir}/applications/%{appid}.desktop
appstream-util validate-relax --nonet %{buildroot}%{_metainfodir}/%{appid}.metainfo.xml || :
glib-compile-schemas --strict --dry-run %{buildroot}%{_datadir}/glib-2.0/schemas

%files
%license LICENSE
%doc README.md
%{_bindir}/tonearm
%{_datadir}/applications/%{appid}.desktop
%{_metainfodir}/%{appid}.metainfo.xml
%{_datadir}/glib-2.0/schemas/%{appid}.gschema.xml
%{_datadir}/icons/hicolor/scalable/apps/%{appid}.svg
%{_datadir}/icons/hicolor/symbolic/apps/%{appid}-symbolic.svg
%{_datadir}/icons/hicolor/16x16/apps/%{appid}.png
%{_datadir}/icons/hicolor/24x24/apps/%{appid}.png
%{_datadir}/icons/hicolor/32x32/apps/%{appid}.png
%{_datadir}/icons/hicolor/48x48/apps/%{appid}.png
%{_datadir}/icons/hicolor/64x64/apps/%{appid}.png
%{_datadir}/icons/hicolor/128x128/apps/%{appid}.png
%{_datadir}/icons/hicolor/256x256/apps/%{appid}.png
%{_datadir}/icons/hicolor/512x512/apps/%{appid}.png

%changelog
* Sat Oct 31 2026 tonearm-rs CI <noreply@github.com> - 1.1.0-1
- Update to 1.1.0

* Sat Oct 31 2026 Nicolas Rodriguez-Alvarez <me@nichokas.eu> - 1.0.0-1
- Initial package of the Rust port, 1.0.0
