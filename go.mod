module github.com/pashifika/comic-auto-resize

go 1.17

require (
	github.com/jessevdk/go-flags v1.5.0
	github.com/mholt/archiver/v4 v4.0.0-alpha.2
	github.com/pashifika/resize v0.0.0-20220113112049-f2d23e2995be
	github.com/pashifika/util v0.2.3
	github.com/pixiv/go-libjpeg v0.0.0-20190822045933-3da21a74767d
	golang.org/x/text v0.3.7
)

require (
	github.com/andybalholm/brotli v1.0.4 // indirect
	github.com/dsnet/compress v0.0.2-0.20210315054119-f66993602bf5 // indirect
	github.com/golang/snappy v0.0.4 // indirect
	github.com/klauspost/compress v1.13.6 // indirect
	github.com/klauspost/pgzip v1.2.5 // indirect
	github.com/nwaples/rardecode/v2 v2.0.0-beta.2 // indirect
	github.com/pierrec/lz4/v4 v4.1.12 // indirect
	github.com/pkg/errors v0.9.1 // indirect
	github.com/therootcompany/xz v1.0.1 // indirect
	github.com/ulikunitz/xz v0.5.10 // indirect
	golang.org/x/net v0.0.0-20211216030914-fe4d6282115f // indirect
	golang.org/x/sys v0.0.0-20210423082822-04245dca01da // indirect
)

replace github.com/mholt/archiver/v4 v4.0.0-alpha.2 => ../archiver/
