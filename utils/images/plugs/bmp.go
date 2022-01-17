// Package plugs
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
package plugs

import (
	"image"
	"io"

	"golang.org/x/image/bmp"
)

type BMP struct {
}

var (
	_bmpExtensions = []string{".bmp"}
	_bmpHeader     = []byte("BM????\x00\x00\x00\x00")
)

// NewBmp use go std lib to BMP image encoding, decoding
func NewBmp() BMP {
	return BMP{}
}

func (j BMP) Name() string { return ".bmp" }

func (j BMP) HeaderLen() int { return len(_bmpHeader) }

func (j BMP) Extensions() []string { return _bmpExtensions }

func (j BMP) Matched(header []byte) bool { return match(_webpHeader, header) }

// DecodeConfig returns the color model and dimensions of a BMP image without decoding the entire image.
func (j BMP) DecodeConfig(r io.Reader) (config image.Config, err error) {
	return bmp.DecodeConfig(r)
}

// Decode reads a BMP data stream from r and returns decoded image as an image.Image.
// Output image has YCbCr colors or 8bit Grayscale.
func (j BMP) Decode(r io.Reader) (dest image.Image, err error) {
	return bmp.Decode(r)
}
