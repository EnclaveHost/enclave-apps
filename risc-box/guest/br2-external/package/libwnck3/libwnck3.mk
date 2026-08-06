################################################################################
#
# libwnck3
#
################################################################################

LIBWNCK3_VERSION_MAJOR = 3.36
LIBWNCK3_VERSION = $(LIBWNCK3_VERSION_MAJOR).0
LIBWNCK3_SOURCE = libwnck-$(LIBWNCK3_VERSION).tar.xz
LIBWNCK3_SITE = https://download.gnome.org/sources/libwnck/$(LIBWNCK3_VERSION_MAJOR)
LIBWNCK3_LICENSE = LGPL-2.0+
LIBWNCK3_LICENSE_FILES = COPYING
LIBWNCK3_INSTALL_STAGING = YES
LIBWNCK3_DEPENDENCIES = libgtk3 libglib2 xlib_libXres host-pkgconf

# introspection needs to run target binaries; startup-notification is optional
# and drags in more X plumbing than the panel needs here.
LIBWNCK3_CONF_OPTS = \
	-Dinstall_tools=false \
	-Dintrospection=false \
	-Dstartup_notification=disabled \
	-Dgtk_doc=false

$(eval $(meson-package))
