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
	"math"
)

const (
	_defaultWidth = 1280
	//_resizeThreshold = 0.01
)

func autoResize(width, height, ratio float64) (w, h uint) {
	if ratio == 0.7 {
		ratio = _defaultWidth / width
	}
	reW := math.Round(width * ratio)
	reH := math.Round(height * ratio)

	return uint(math.Round(reW)), uint(math.Round(reH))
}
