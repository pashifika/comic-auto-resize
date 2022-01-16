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
	"io"
	"io/fs"
	//osPath "path"
	"path/filepath"
	"strings"
	"sync"

	"github.com/pashifika/compress"
	_ "github.com/pashifika/compress/_7zip"
	_ "github.com/pashifika/compress/rar"
	_ "github.com/pashifika/compress/zip"
	"github.com/pashifika/util/errgroup"
	"github.com/pashifika/util/mem"

	"github.com/pashifika/comic-auto-resize/utils/config"
	"github.com/pashifika/comic-auto-resize/utils/images"
	"github.com/pashifika/comic-auto-resize/utils/log"
)

type FileSystem struct {
	_mu        sync.Mutex
	compressor *compress.FileSystem
	archivers  []file
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
	vfs := &compress.FileSystem{
		Charset:     conf.AutoEncodings,
		SkipCharErr: false,
	}
	return &FileSystem{
		compressor: vfs,
		savePath:   conf.OutPut,
		deleteOrg:  conf.DeleteInput,
	}
}

// Open is set archiver path or local directory path
func (f *FileSystem) Open(ctx context.Context, root string, eg *errgroup.Group) error {
	fsys, err := f.compressor.Open(root)
	if err != nil {
		return err
	}
	f.archivers = []file{}
	f.rootName = filepath.Base(root)
	err = fs.WalkDir(fsys, ".", func(path string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if d.IsDir() {
			switch path {
			case ".git":
				return fs.SkipDir
			case compress.DefaultArchiverRoot:
				return nil
			default:
				var info fs.FileInfo
				info, err = d.Info()
				if err != nil {
					return err
				}
				if info.Name() == f.rootName {
					return nil
				}
				f.archivers = append(f.archivers, file{
					FileInfo: info,
					root:     path,
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
					buf := &mem.FakeIO{}
					buf.Grow(int(info.Size()))
					image := &images.ImageData{FakeIO: buf}
					f.archivers = append(f.archivers, file{
						FileInfo: info,
						root:     path,
						read:     image.Read,
						write:    image.Write,
						close:    image.Close,
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
			_ = f.compressor.Close()
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
				af, err := fsys.Open(file.root)
				if err != nil {
					return err
				}
				image := f.Cache.GetImageBuffer(file.root)
				n, err := io.Copy(image, af)
				_ = af.Close()
				if err != nil {
					return err
				}
				if n != image.Size() {
					return ErrArchiverReadSize
				}
				f.Cache.Send() <- file.root
			}
		}
		return nil
	})

	return err
}

var (
	ErrArchiverReadSize = errors.New("archiver file error, can not read all bytes")
)
