# Read before every makefile through the MAKEFILES of the local work guard: refuses make on this host. The shell call
# prints the refusal text to stderr while make parses this file, and the error then stops make.
$(shell bash $(dir $(lastword $(MAKEFILE_LIST)))../refuse.sh make 1>&2)
$(error make compiles or runs tests on this host)
