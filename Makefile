MAKEFILE_DIR := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))
OS           := win
ARCH         := amd64

release-win:
	set LD_LIBRARY_PATH=-L$(MAKEFILE_DIR)mozjpeg/win/lib_$(ARCH)
	set CGO_LDFLAGS=-L$(MAKEFILE_DIR)mozjpeg/win/lib_$(ARCH)
	set CGO_CPPFLAGS=-I$(MAKEFILE_DIR)mozjpeg/win/include
	set CGO_ENABLED=1
	set GOARCH=$(ARCH)
	go build -ldflags "-s -w -extldflags=-static" -tags=release .

release-mac:
	LD_LIBRARY_PATH=-L$(MAKEFILE_DIR)mozjpeg/mac/lib_$(ARCH) \
	CGO_LDFLAGS=-L$(MAKEFILE_DIR)mozjpeg/mac/lib_$(ARCH) \
	CGO_CPPFLAGS=-I$(MAKEFILE_DIR)mozjpeg/mac/include \
	CGO_ENABLED=1 GOARCH=$(ARCH) \
	go build -ldflags "-s -w" -tags=release .

## show help
help:
	@make2help $(MAKEFILE_LIST)
