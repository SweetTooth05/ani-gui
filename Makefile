# ani-gui — build and install the native Linux app.
#
#   make                          build the release binary
#   make install                  install into ~/.local (no root needed)
#   make install PREFIX=/usr/local
#   make uninstall                remove exactly what install put there

PREFIX  ?= $(HOME)/.local
DESTDIR ?=
APP_ID  := io.github.sweettooth05.AniGui
BIN     := target/release/ani-gui

ROOT     = $(DESTDIR)$(PREFIX)
BINDIR   = $(ROOT)/bin
APPSDIR  = $(ROOT)/share/applications
ICONDIR  = $(ROOT)/share/icons/hicolor

ICON_SIZES := 16x16 22x22 24x24 32x32 48x48 64x64 128x128 256x256 512x512
# Icon files relative to assets/icons/hicolor and to $(ICONDIR).
ICONS := $(foreach s,$(ICON_SIZES),$(s)/apps/$(APP_ID).png) \
	scalable/apps/$(APP_ID).svg \
	symbolic/apps/$(APP_ID)-symbolic.svg

.PHONY: all build install uninstall

all: build

build:
	cargo build --release

# Built only when missing, so `sudo make install` after `make` never runs
# cargo as root. It does not track sources: run `make` before `make install`
# after pulling changes, or the old binary is installed.
$(BIN):
	cargo build --release

install: $(BIN)
	install -Dm755 $(BIN) $(BINDIR)/ani-gui
	install -Dm644 assets/ani-gui.desktop $(APPSDIR)/$(APP_ID).desktop
	@for icon in $(ICONS); do \
		echo "install -Dm644 assets/icons/hicolor/$$icon $(ICONDIR)/$$icon"; \
		install -Dm644 assets/icons/hicolor/$$icon $(ICONDIR)/$$icon || exit 1; \
	done
	@# Staged installs (DESTDIR) leave cache refreshes to the package manager.
	-@[ -n "$(DESTDIR)" ] || ! command -v gtk-update-icon-cache >/dev/null 2>&1 || \
		gtk-update-icon-cache -f -t $(ICONDIR) >/dev/null 2>&1 || true
	-@[ -n "$(DESTDIR)" ] || ! command -v update-desktop-database >/dev/null 2>&1 || \
		update-desktop-database $(APPSDIR) >/dev/null 2>&1 || true

# The caches install refreshed are rebuilt for whatever other apps remain, or
# removed when nothing else is left in that directory.
uninstall:
	rm -f $(BINDIR)/ani-gui
	rm -f $(APPSDIR)/$(APP_ID).desktop
	rm -f $(addprefix $(ICONDIR)/,$(ICONS))
	-@if [ -d $(ICONDIR) ]; then \
		if [ -z "$$(find $(ICONDIR) -type f ! -name icon-theme.cache -print -quit)" ]; then \
			rm -f $(ICONDIR)/icon-theme.cache; \
		elif command -v gtk-update-icon-cache >/dev/null 2>&1; then \
			gtk-update-icon-cache -f -t $(ICONDIR) >/dev/null 2>&1 || true; \
		fi; \
	fi
	-@if [ -d $(APPSDIR) ]; then \
		if [ -z "$$(find $(APPSDIR) -name '*.desktop' -print -quit)" ]; then \
			rm -f $(APPSDIR)/mimeinfo.cache; \
		elif command -v update-desktop-database >/dev/null 2>&1; then \
			update-desktop-database $(APPSDIR) >/dev/null 2>&1 || true; \
		fi; \
	fi
