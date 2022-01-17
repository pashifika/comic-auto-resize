// Package stream
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
package stream

import (
	"context"
	"errors"
	"io"
	"sync"

	"github.com/pashifika/util/errgroup"

	"github.com/pashifika/comic-auto-resize/utils/images"
	"github.com/pashifika/comic-auto-resize/utils/log"
)

type Stream struct {
	_mu    sync.Mutex
	wg     sync.WaitGroup
	images map[string]*images.ImageData
	send   chan string

	File  archiverFile
	Image images.Images
}

type archiverFile interface {
}

func (s *Stream) Init(cpus int) {
	s.images = map[string]*images.ImageData{}
	s.send = make(chan string, cpus)
}

// NewImage save to image data map(root is in archiver file path)
func (s *Stream) NewImage(image *images.ImageData, root string) {
	s._mu.Lock()
	s.images[root] = image
	s._mu.Unlock()
}

// GetImageBuffer is get image bytes
func (s *Stream) GetImageBuffer(root string) *images.ImageData {
	s._mu.Lock()
	defer s._mu.Unlock()
	return s.images[root]
}

// Send is request to image conversion process
func (s *Stream) Send() chan<- string { return s.send }

// WaitGroupAdd see sync.WaitGroup Add
func (s *Stream) WaitGroupAdd(delta int) { s.wg.Add(delta) }

// WaitGroupDone see sync.WaitGroup Done
func (s *Stream) WaitGroupDone() { s.wg.Done() }

func (s *Stream) CloseSend() { close(s.send) }

func (s *Stream) Convert(ctx context.Context, eg *errgroup.Group) error {
	s.wg.Add(1)
	go func() {
		defer s.wg.Done()
		for root := range s.send {
			select {
			case <-ctx.Done():
				err := ctx.Err()
				if errors.Is(err, context.Canceled) {
					log.Debug("convert is canceled")
					return
				}
				return
			default:
				root := root
				eg.Go(func() error {
					buf := s.GetImageBuffer(root)
					err := s.Image.Identify(root, buf)
					if err != nil {
						return errWrap("image.Identify", root, err)
					}
					_, _ = buf.Seek(0, io.SeekStart)
					src, err := s.Image.Decoder(root, buf)
					buf.Reset() // delete cache image file
					if err != nil {
						return errWrap("image.Decoder", root, err)
					}
					src, err = s.Image.Resize(root, src)
					if err != nil {
						return errWrap("image.Resize", root, err)
					}
					err = s.Image.Encoder(buf, src)
					if err != nil {
						return errWrap("image.Encoder", root, err)
					}
					return nil
				})
			}
		}
	}()

	s.wg.Wait()
	return eg.Wait()
}

func errWrap(fn, root string, err error) error {
	return errors.New(fn + ": " + root + " error, " + err.Error())
}
