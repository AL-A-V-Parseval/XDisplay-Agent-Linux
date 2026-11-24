CARGO ?= cargo
PYTHON ?= /usr/bin/python3
PKG_CONFIG ?= pkg-config
CFLAGS ?= -O2 -std=c11 -Wall -Wextra -Werror
HOST := linux-host
MANIFEST := $(HOST)/rust-backend/Cargo.toml
BUILD := $(HOST)/rust-backend/target
DRM_CFLAGS = $(shell $(PKG_CONFIG) --cflags libdrm)
DRM_LIBS = $(shell $(PKG_CONFIG) --libs libdrm)

.PHONY: all backend run test
all: backend $(HOST)/vkms-read $(HOST)/vkms-probe $(HOST)/turbo-jpeg

backend:
	$(CARGO) build --locked --release --manifest-path $(MANIFEST) --target-dir $(BUILD)
	install -m 755 $(BUILD)/release/xdisplay-backend $(HOST)/xdisplay-backend

$(HOST)/vkms-read: $(HOST)/vkms_read.c $(HOST)/cursor_compose.h $(HOST)/cursor_stage_probe.h $(HOST)/raw_slot.h
	$(CC) $(CFLAGS) $(DRM_CFLAGS) $< -o $@ $(DRM_LIBS)

$(HOST)/vkms-probe: $(HOST)/vkms_probe.c
	$(CC) $(CFLAGS) $(DRM_CFLAGS) $< -o $@ $(DRM_LIBS)

$(HOST)/turbo-jpeg: $(HOST)/turbo_jpeg.c
	$(CC) $(CFLAGS) $< -o $@ -lturbojpeg

run: all
	$(PYTHON) -B $(HOST)/panel.py

test: all
	$(CARGO) test --locked --all-features --manifest-path $(MANIFEST) --target-dir $(BUILD)
	PYTHONDONTWRITEBYTECODE=1 $(PYTHON) -B -m unittest discover -s $(HOST) -p 'test_panel*.py'
