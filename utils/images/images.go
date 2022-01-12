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
	"image"

	"github.com/pashifika/util/mem"

	"github.com/pashifika/comic-auto-resize/utils/config"
	"github.com/pashifika/comic-auto-resize/utils/images/plugs"
	"github.com/pashifika/comic-auto-resize/utils/log"
)

type Processing struct {
}

func NewImageProcess() *Processing {
	return &Processing{}
}

func (p *Processing) Init(conf config.Config) {
	jpeg := plugs.NewJpeg(
		conf.Quality, conf.Jpeg.OptimizeCoding, conf.Jpeg.ProgressiveMode,
		conf.Jpeg.DCTMethod,
	)
	RegisterDecoder(jpeg)

	RegisterEncoder(jpeg)
}

func (p *Processing) Identify(path string, buf mem.FakeReader) error {
	for name, decoder := range decoders {
		headerHex := make([]byte, decoder.HeaderLen())
		_, err := buf.ReadAt(headerHex, 0)
		if err != nil {
			log.Debug("%s image[%s] header error", path, name)
			continue
		}
		if !decoder.Matched(headerHex) {
			continue
		}
		cfg, err := decoder.DecodeConfig(buf)
		log.Debug("path:%s, image.Config:%v", path, cfg)
	}

	return nil
}

func (p *Processing) Extensions() []string {
	var extList []string
	for _, decoder := range decoders {
		extList = append(extList, decoder.Extensions()...)
	}
	return extList
}

func (p *Processing) Resize(src image.Image) (dest image.Image, err error) {
	return nil, err
}
