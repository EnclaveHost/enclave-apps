################################################################################
#
# xfdesktop
#
################################################################################

XFDESKTOP_VERSION = 4.18.1
XFDESKTOP_SOURCE = xfdesktop-$(XFDESKTOP_VERSION).tar.bz2
XFDESKTOP_SITE = https://archive.xfce.org/src/xfce/xfdesktop/$(basename $(XFDESKTOP_VERSION))
XFDESKTOP_LICENSE = GPL-2.0+
XFDESKTOP_LICENSE_FILES = COPYING
XFDESKTOP_INSTALL_STAGING = YES
XFDESKTOP_DEPENDENCIES = libxfce4ui libxfce4util garcon exo libwnck3 libgtk3
XFDESKTOP_CONF_OPTS = --disable-notifications

$(eval $(autotools-package))
