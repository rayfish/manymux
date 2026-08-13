#!/bin/sh
#
# PROVIDE: @RCNAME@
# REQUIRE: NETWORKING
# KEYWORD: shutdown
#
# @DESC@. Written by `tiles service install`.
# BSD rc.d has no per-user services, so this drops to @USER@ before running.

. /etc/rc.subr

name="@RCNAME@"
rcvar="@RCNAME@_enable"
desc="@DESC@"

load_rc_config $name
: ${@RCNAME@_enable:="NO"}
: ${@RCNAME@_user:="@USER@"}

pidfile="/var/run/${name}.pid"
procname="@BIN@"
command="/usr/sbin/daemon"
command_args="-P ${pidfile} -r -f -u ${@RCNAME@_user} \
	/usr/bin/env TILES_CONFIG_DIR=@CONFIG@ @BIN@ @COMMAND@"

run_rc_command "$1"
