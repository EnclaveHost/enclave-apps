################################################################################
#
# exo
#
################################################################################

EXO_VERSION = 4.18.0
EXO_SOURCE = exo-$(EXO_VERSION).tar.bz2
EXO_SITE = https://archive.xfce.org/src/xfce/exo/$(basename $(EXO_VERSION))
EXO_LICENSE = GPL-2.0+, LGPL-2.1+
EXO_LICENSE_FILES = COPYING
EXO_INSTALL_STAGING = YES
EXO_DEPENDENCIES = libxfce4util libxfce4ui libgtk3
EXO_CONF_OPTS = --disable-gtk-doc

$(eval $(autotools-package))
