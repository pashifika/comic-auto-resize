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
	"io"
	"strconv"
	"sync"

	"github.com/pashifika/resize"
	"github.com/pashifika/util/mem"

	"github.com/pashifika/comic-auto-resize/utils/config"
	"github.com/pashifika/comic-auto-resize/utils/images/plugs"
	"github.com/pashifika/comic-auto-resize/utils/log"
)

type Processing struct {
	_mu        sync.RWMutex
	imageInfo  map[string]imageInfo // key is file root
	resizeMode resize.InterpolationFunction

	ratio int
}

type imageInfo struct {
	engine string
	conf   image.Config
}

func (i imageInfo) String() string {
	return "{engine:" + i.engine +
		", w:" + strconv.Itoa(i.conf.Width) +
		", h:" + strconv.Itoa(i.conf.Height) + "}"
}

func NewImageProcess() *Processing {
	return &Processing{_mu: sync.RWMutex{}, imageInfo: map[string]imageInfo{}}
}

func (p *Processing) Init(conf config.Config) {
	p.ratio = conf.Ratio
	p.resizeMode = conf.ResizeMode
	jpeg := plugs.NewJpeg(
		conf.Quality, conf.Jpeg.OptimizeCoding, conf.Jpeg.ProgressiveMode,
		conf.Jpeg.DCTMethod,
	)
	RegisterDecoder(jpeg)
	RegisterDecoder(plugs.NewBmp())
	RegisterDecoder(plugs.NewPng())
	RegisterDecoder(plugs.NewWebp())

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
		if err != nil {
			continue
		}
		p._mu.Lock()
		p.imageInfo[path] = imageInfo{engine: name, conf: cfg}
		p._mu.Unlock()
		log.Debug("path: %s  image.Config=%s", path, p.imageInfo[path])
		return nil
	}
	return ErrUnknownImage
}

func (p *Processing) Decoder(path string, r io.Reader) (image.Image, error) {
	p._mu.RLock()
	info, ok := p.imageInfo[path]
	p._mu.RUnlock()
	if !ok {
		return nil, ErrUnknownIdentify
	}
	decoder, ok := decoders[info.engine]
	if !ok {
		return nil, ErrUnknownDecoder
	}
	return decoder.Decode(r)
}

func (p *Processing) Resize(path string, src image.Image) (image.Image, error) {
	if p.ratio == 100 {
		return src, nil
	}
	p._mu.RLock()
	info, ok := p.imageInfo[path]
	if !ok {
		p._mu.RUnlock()
		return nil, ErrUnknownIdentify
	}
	width := info.conf.Width
	height := info.conf.Height
	p._mu.RUnlock()
	var reW, reH uint
	doResize := true
	if p.ratio == 70 && _defaultWidth >= width {
		reW = uint(width)
		reH = uint(height)
		doResize = false
	} else {
		reW, reH = autoResize(float64(width), float64(height), float64(p.ratio)/100)
	}
	if reW <= 500 || reH <= 100 {
		log.Warn("path: %s width=%d height=%d, %s", path, width, height, ErrRatioValueSmall.Error())
		doResize = false
	}
	log.Debug("path: %s  resize to: width=%d height=%d", path, reW, reH)

	if doResize {
		return resize.Resize(reW, reH, src, p.resizeMode), nil
	}
	return src, nil
}

func (p *Processing) Encoder(w io.Writer, src image.Image) error {
	if encoder == nil {
		log.Panic("not find encoder")
	}
	return encoder.Encode(w, src)
}

func (p *Processing) Extensions() []string {
	var extList []string
	for _, decoder := range decoders {
		extList = append(extList, decoder.Extensions()...)
	}
	return extList
}

func (p *Processing) TargetExtension() string {
	return encoder.Name()
}
