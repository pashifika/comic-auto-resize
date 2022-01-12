// Package archiver
/*
 * Version: 1.0.0
 * Copyright (c) 2022. Pashifika
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */
package archiver

import (
	"context"
	"errors"
	"io/fs"
	//osPath "path"
	"path/filepath"
	"strings"
	"sync"

	"github.com/mholt/archiver/v4"
	"github.com/pashifika/util/mem"

	"github.com/pashifika/comic-auto-resize/utils/config"
	"github.com/pashifika/comic-auto-resize/utils/errgroup"
	"github.com/pashifika/comic-auto-resize/utils/images"
	"github.com/pashifika/comic-auto-resize/utils/log"
)

type FileSystem struct {
	fsys       fs.FS
	_mu        sync.Mutex
	compressor archiver.Archiver
	archivers  []archiver.File
	rootName   string
	savePath   string
	deleteOrg  bool

	Cache      stream
	Extensions []string
}

type stream interface {
	NewImage(image *images.ImageData, root string)
	GetImageBuffer(root string) *images.ImageData
	Send() chan<- string
	WaitGroupAdd(delta int)
	WaitGroupDone()
	CloseSend()
}

func NewFileSystem(conf config.Config) *FileSystem {
	archiver.AutoEncodings = conf.AutoEncodings
	return &FileSystem{
		savePath:  conf.OutPut,
		deleteOrg: conf.DeleteInput,
	}
}

// Open is set archiver path or local directory path
func (f *FileSystem) Open(root string) (err error) {
	f.fsys, err = archiver.FileSystem(root)
	if err == nil {
		f._mu = sync.Mutex{}
		f.rootName = filepath.Base(root)
		f.archivers = []archiver.File{}
	}
	return
}

func (f *FileSystem) FindAll(ctx context.Context, eg *errgroup.Group) error {
	// Generate a list of files to be converted
	err := fs.WalkDir(f.fsys, ".", func(path string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		//nameInArchive := osPath.Join(rootInArchive, strings.TrimPrefix(filename, rootOnDisk))
		if d.IsDir() {
			switch path {
			case ".git":
				return fs.SkipDir
			default:
				var info fs.FileInfo
				info, err = d.Info()
				if err != nil {
					return err
				}
				if info.Name() == f.rootName {
					return nil
				}
				f.archivers = append(f.archivers, archiver.File{
					FileInfo:      info,
					NameInArchive: path,
					Open:          nil,
				})
			}
		} else {
			// Image file filter
			for _, ext := range f.Extensions {
				if ext == filepath.Ext(strings.ToLower(path)) {
					var info fs.FileInfo
					info, err = d.Info()
					if err != nil {
						return err
					}
					image := &images.ImageData{
						SingleFakeIO: mem.NewSingleFakeIO(info.Size(), false),
					}
					f.archivers = append(f.archivers, archiver.File{
						FileInfo:      info,
						NameInArchive: path,
						Open:          image.Open,
					})
					f.Cache.NewImage(image, path)
				}
			}
		}
		return nil
	})

	// Read the file and write to the cache system
	f.Cache.WaitGroupAdd(1)
	eg.Go(func() error {
		defer func() {
			f.Cache.WaitGroupDone()
			f.Cache.CloseSend()
		}()
		for _, file := range f.archivers {
			select {
			case <-ctx.Done():
				err := ctx.Err()
				if errors.Is(err, context.Canceled) {
					log.Debug("file system is canceled")
					return nil
				}
				return err
			default:
				if file.IsDir() {
					continue
				}
				af, err := f.fsys.Open(file.NameInArchive)
				if err != nil {
					return err
				}
				image := f.Cache.GetImageBuffer(file.NameInArchive)
				_, err = af.Read(image.Buf)
				if err != nil {
					return err
				}
				f.Cache.Send() <- file.NameInArchive
			}
		}
		return nil
	})

	return err
}
