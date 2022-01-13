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
	"errors"
	"fmt"
	"os"

	"github.com/jessevdk/go-flags"
	"github.com/pashifika/resize"
	"github.com/pashifika/util/files"
)

type Config struct {
	AutoEncodings charset
	Input         string
	DeleteInput   bool
	OutPut        string
	Ratio         int
	Quality       int
	ResizeMode    resize.InterpolationFunction
	Jpeg          imageJpeg
	Developer     developer
}

type options struct {
	AutoEncodings charset       `long:"charset" default:"ja,zh" optional:"yes" description:"decode zip file charset."`
	OutPut        path          `short:"o" long:"out" description:"set output file path, not empty to delete original file."`
	Ratio         pctInt        `short:"r" long:"ratio" default:"70" description:"set resize ratio."`
	Quality       pctInt        `short:"q" long:"quality" default:"90" description:"set encoder quality."`
	ResizeMode    interpolation `long:"resize-mode" default:"lanczos3" description:"set resize interpolation mode." long-description:"supported the: nearest-neighbor, bilinear, bicubic, mitchell-netravali, lanczos2, lanczos3."`
	Jpeg          imageJpeg     `group:"Jpeg Options"`
	Developer     developer     `group:"Developer Options"`
}

type imageJpeg struct {
	OptimizeCoding  bool      `long:"optimizer" default-mask:"true" description:"perform optimization of entropy encoding parameters."`
	ProgressiveMode bool      `long:"progressive" default-mask:"true" description:"create progressive JPEG file."`
	DCTMethod       DCTMethod `long:"dct" default:"float" choice:"float" choice:"ifast" choice:"islow" description:"set JPEG encoder DCT/IDCT method." long-description:"FLOAT is floating-point: accurate, fast on fast HW. IFAST is faster, less accurate integer method. ISLOW is slow but accurate integer algorithm."`
}

type developer struct {
	Debug bool `long:"debug" default-mask:"false" description:"enable debug mode"`
}

func InitFlags() Config {
	opts := options{
		OutPut:  path{CheckExists: true},
		Ratio:   pctInt{Default: 70},
		Quality: pctInt{Default: 90},
	}
	parser := flags.NewParser(&opts, flags.Default)
	args, err := parser.Parse()
	if err != nil {
		panic(err)
	}
	parser.Usage = "[OPTIONS] (archiver file / directory)"

	// Parser input file
	if len(args) != 1 {
		parser.WriteHelp(os.Stderr)
		os.Exit(flags.HelpFlag)
	}
	input := args[0]
	if !files.Exists(input) {
		_, _ = os.Stderr.WriteString(fmt.Sprintf("%s does not exist", input))
		os.Exit(flags.IgnoreUnknown)
	}

	// Parser OutPutDir
	var deleteInput bool
	outPut := opts.OutPut.Value
	if opts.OutPut.Value == "" {
		deleteInput = true
		outPut = input
	}

	return Config{
		AutoEncodings: opts.AutoEncodings,
		Input:         input,
		DeleteInput:   deleteInput,
		OutPut:        outPut,
		Ratio:         opts.Ratio.Value,
		Quality:       opts.Quality.Value,
		ResizeMode:    opts.ResizeMode.Value,
		Jpeg:          opts.Jpeg,
		Developer:     opts.Developer,
	}
}

var (
	ErrUnknownType  = errors.New("unknown type")
	ErrNotSupported = errors.New("not supported")
)
