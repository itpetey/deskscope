# Deskscope

Kiosk GUI for displaying a Raspberry Pi camera feed

## Build

```bash
cross build --release --target aarch64-unknown-linux-gnu
```

## Install

```bash
# Install binary
sudo cp target/aarch64-unknown-linux-gnu/release/deskscope /usr/local/bin/
sudo chmod +x /usr/local/bin/deskscope

# Install config file
sudo cp assets/deskscope.conf /usr/local/etc/deskscope.conf

# Install wrapper script (auto-detects Wayland/X11 display)
sudo cp scripts/run-deskscope /usr/local/bin/
sudo chmod +x /usr/local/bin/run-deskscope

# Create autostart entry
mkdir -p ~/.config/autostart
cat > ~/.config/autostart/deskscope.desktop <<'EOF'
[Desktop Entry]
Type=Application
Name=Deskscope
Exec=/usr/local/bin/run-deskscope
Terminal=false
EOF

# Rotate screen
cat > ~/.config/autostart/rotate.desktop <<'EOF'
[Desktop Entry]
Type=Application
Name=Rotate
Exec=/usr/bin/wlr-randr --output HDMI-A-1 --transform 270
Terminal=true
EOF
```

### Running manually (e.g. over SSH)

The binary needs a display to connect to. Source your session environment first:

```bash
# For Wayland:
export XDG_RUNTIME_DIR=/run/user/$(id -u)
export WAYLAND_DISPLAY=wayland-0    # or wayland-1; check: ls $XDG_RUNTIME_DIR/wayland-*

# For X11:
export DISPLAY=:0

deskscope
```

## OS Image Setup

```bash
cp assets/waveshare-ads7846-overlay.dts /Volumes/bootfs/overlays/
cat <<EOF
# Shutdown pin
# dtoverlay=gpio-shutdown,gpio_pin=21

# Touchscreen settings
hdmi_group=2
hdmi_mode=87
hdmi_cvt 800 480 60 6 0 0 0
hdmi_drive=1
dtoverlay=waveshare-ads7846,penirq=25,xmin=200,xmax=3900,ymin=200,ymax=3900,speed=50000
EOF >> /boot/config.txt
```
