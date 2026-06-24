# Deskscope

GUI for interacting with a Raspberry Pi camera.

## Usage

```bash
cross build --release --target aarch64-unknown-linux-gnu
```

## OS Setup

```bash
echo "dtoverlay=gpio-shutdown,gpio_pin=26" >> /boot/config.txt
```
