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

	"golang.org/x/image/webp"
)

type WEBP struct {
}

var (
	_webpExtensions = []string{".webp"}
	_webpHeader     = "RIFF????WEBPVP8"
)

// NewWebp use go std lib to WEBP image encoding, decoding
func NewWebp() WEBP {
	return WEBP{}
}

func (j WEBP) Name() string { return ".webp" }

func (j WEBP) HeaderLen() int { return len(_webpHeader) }

func (j WEBP) Extensions() []string { return _webpExtensions }

func (j WEBP) Matched(header []byte) bool { return match(_webpHeader, header) }

// DecodeConfig returns the color model and dimensions of a WEBP image without decoding the entire image.
func (j WEBP) DecodeConfig(r io.Reader) (config image.Config, err error) {
	return webp.DecodeConfig(r)
}

// Decode reads a WEBP data stream from r and returns decoded image as an image.Image.
// Output image has YCbCr colors or 8bit Grayscale.
func (j WEBP) Decode(r io.Reader) (dest image.Image, err error) {
	return webp.Decode(r)
}

// Match reports whether magic matches b. Magic may contain "?" wildcards.
func match(magic string, b []byte) bool {
	if len(magic) != len(b) {
		return false
	}
	for i, c := range b {
		if magic[i] != c && magic[i] != '?' {
			return false
		}
	}
	return true
}
