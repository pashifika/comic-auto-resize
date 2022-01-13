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
	"github.com/nfnt/resize"
)

type interpolation struct {
	Value resize.InterpolationFunction
}

func (i *interpolation) UnmarshalFlag(value string) error {
	var ifu resize.InterpolationFunction
	switch value {
	case "nearest-neighbor":
		ifu = resize.NearestNeighbor
	case "bilinear":
		ifu = resize.Bilinear
	case "bicubic":
		ifu = resize.Bicubic
	case "mitchell-netravali":
		ifu = resize.MitchellNetravali
	case "lanczos2":
		ifu = resize.Lanczos2
	case "lanczos3":
		ifu = resize.Lanczos3
	default:
		return ErrNotSupported
	}
	*i = interpolation{Value: ifu}
	return nil
}

func (i interpolation) MarshalFlag() (str string, err error) {
	switch i.Value {
	case resize.NearestNeighbor:
		str = "nearest-neighbor"
	case resize.Bilinear:
		str = "bilinear"
	case resize.Bicubic:
		str = "bicubic"
	case resize.MitchellNetravali:
		str = "mitchell-netravali"
	case resize.Lanczos2:
		str = "lanczos2"
	case resize.Lanczos3:
		str = "lanczos3"
	default:
		err = ErrUnknownType
	}
	return
}
