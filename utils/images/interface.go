// Package images
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
package images

import (
	"errors"
	"image"
	"io"

	"github.com/pashifika/util/mem"
)

type Images interface {
	Identify(path string, buf mem.FakeReader) error
	Decoder(path string, r io.Reader) (image.Image, error)
	Resize(path string, src image.Image) (image.Image, error)
	Encoder(w io.Writer, src image.Image) error
}

type Decoder interface {
	Name() string
	HeaderLen() int
	Extensions() []string
	DecodeConfig(r io.Reader) (config image.Config, err error)
	Decode(r io.Reader) (dest image.Image, err error)
	MatchResult
}

type Encoder interface {
	Encode(w io.Writer, src image.Image) (err error)
}

type MatchResult interface {
	Matched(header []byte) bool
}

// errors

var (
	ErrUnknownDecoder  = errors.New("unknown decoder")
	ErrUnknownIdentify = errors.New("unknown identify")
	ErrUnknownImage    = errors.New("unknown image")
	ErrRatioValueSmall = errors.New("resize ratio value too small")
)
