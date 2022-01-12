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
	"strings"
)

// RegisterDecoder registers decoder. It should be called during init.
// Duplicate decoders by name are not allowed and will panic.
func RegisterDecoder(decoder Decoder) {
	name := strings.TrimLeft(strings.ToLower(decoder.Name()), ".")
	if _, ok := decoders[name]; ok {
		panic("decoder " + name + " is already registered")
	}
	decoders[name] = decoder
}

// RegisterEncoder registers encoder.
func RegisterEncoder(encode Encoder) {
	encoder = encode
}

// Registered Decoder.
var decoders = make(map[string]Decoder)

// Registered Encoder.
var encoder Encoder
