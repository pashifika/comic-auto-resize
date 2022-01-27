// Package config
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
package config

import (
	"github.com/pashifika/go-libjpeg/jpeg"
)

type DCTMethod jpeg.DCTMethod

func (dct *DCTMethod) UnmarshalFlag(value string) error {
	switch value {
	case "float":
		*dct = (DCTMethod)(jpeg.DCTFloat)
	case "ifast":
		*dct = (DCTMethod)(jpeg.DCTIFast)
	case "islow":
		*dct = (DCTMethod)(jpeg.DCTISlow)
	default:
		return ErrNotSupported
	}
	return nil
}

func (dct DCTMethod) MarshalFlag() (string, error) {
	var (
		str string
		err error
	)
	switch (jpeg.DCTMethod)(dct) {
	case jpeg.DCTFloat:
		str = "float"
	case jpeg.DCTIFast:
		str = "ifast"
	case jpeg.DCTISlow:
		str = "islow"
	default:
		err = ErrUnknownType
	}
	return str, err
}
