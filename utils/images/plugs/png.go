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
	"bytes"
	"image"
	"image/png"
	"io"
)

type PNG struct {
}

var (
	_pngExtensions = []string{".png"}
	_pngHeader     = []byte("\x89PNG\r\n\x1a\n")
)

// NewPng use go std lib to PNG image encoding, decoding
func NewPng() PNG {
	return PNG{}
}

func (j PNG) Name() string { return ".png" }

func (j PNG) HeaderLen() int { return len(_pngHeader) }

func (j PNG) Extensions() []string { return _pngExtensions }

func (j PNG) Matched(header []byte) bool {
	return bytes.Equal(_pngHeader, header)
}

// DecodeConfig returns the color model and dimensions of a PNG image without decoding the entire image.
func (j PNG) DecodeConfig(r io.Reader) (config image.Config, err error) {
	return png.DecodeConfig(r)
}

// Decode reads a PNG data stream from r and returns decoded image as an image.Image.
// Output image has YCbCr colors or 8bit Grayscale.
func (j PNG) Decode(r io.Reader) (dest image.Image, err error) {
	return png.Decode(r)
}
