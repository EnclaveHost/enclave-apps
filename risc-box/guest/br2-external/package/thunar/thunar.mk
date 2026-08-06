################################################################################
#
# thunar
#
################################################################################

THUNAR_VERSION = 4.18.11
THUNAR_SOURCE = thunar-$(THUNAR_VERSION).tar.bz2
THUNAR_SITE = https://archive.xfce.org/src/xfce/thunar/$(basename $(THUNAR_VERSION))
THUNAR_LICENSE = GPL-2.0+, LGPL-2.1+
THUNAR_LICENSE_FILES = COPYING
THUNAR_INSTALL_STAGING = YES
THUNAR_DEPENDENCIES = libxfce4ui libxfce4util exo libgtk3
THUNAR_CONF_OPTS = --disable-gtk-doc --disable-notifications --disable-gudev --disable-exif

$(eval $(autotools-package))
