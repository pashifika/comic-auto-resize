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
	"io"

	"github.com/pashifika/go-libjpeg/jpeg"

	"github.com/pashifika/comic-auto-resize/utils/config"
)

type JPEG struct {
	Options jpeg.EncoderOptions
}

var (
	_jpegExtensions = []string{".jpg", ".jpeg"}
	_jpegHeader     = []byte("\xff\xd8")
)

// NewJpeg use mozjpeg to JPEG image encoding, decoding
func NewJpeg(quality int, optimizer, progressive bool, dct config.DCTMethod) JPEG {
	return JPEG{
		Options: jpeg.EncoderOptions{
			Quality:         quality,
			OptimizeCoding:  optimizer,
			ProgressiveMode: progressive,
			DCTMethod:       (jpeg.DCTMethod)(dct),
		},
	}
}

func (j JPEG) Name() string { return ".jpg" }

func (j JPEG) HeaderLen() int { return len(_jpegHeader) }

func (j JPEG) Extensions() []string { return _jpegExtensions }

func (j JPEG) Matched(header []byte) bool {
	return bytes.Equal(_jpegHeader, header)
}

// DecodeConfig returns the color model and dimensions of a JPEG image without decoding the entire image.
func (j JPEG) DecodeConfig(r io.Reader) (config image.Config, err error) {
	return jpeg.DecodeConfig(r)
}

// Decode reads a JPEG data stream from r and returns decoded image as an image.Image.
// Output image has YCbCr colors or 8bit Grayscale.
func (j JPEG) Decode(r io.Reader) (dest image.Image, err error) {
	return jpeg.Decode(r, &jpeg.DecoderOptions{DCTMethod: j.Options.DCTMethod})
}

// Encode encodes src image and writes into w as JPEG format data.
func (j JPEG) Encode(w io.Writer, src image.Image) (err error) {
	return jpeg.Encode(w, src, &j.Options)
}
