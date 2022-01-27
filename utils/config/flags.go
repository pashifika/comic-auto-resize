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
	"os"
	"path/filepath"
	"strings"

	"github.com/jessevdk/go-flags"
	"github.com/pashifika/resize"
	"github.com/pashifika/util/files"

	"github.com/pashifika/comic-auto-resize/utils/log"
)

type Config struct {
	AutoEncodings charset
	Input         string
	Passwd        string
	DeleteInput   bool
	Output        string
	Ratio         int
	Quality       int
	ResizeMode    resize.InterpolationFunction
	Jpeg          imageJpeg
	Developer     developer
}

type options struct {
	AutoEncodings charset       `long:"charset" default:"ja,zh" optional:"yes" description:"decode zip file charset."`
	DeleteInput   bool          `long:"delete-org" default-mask:"false" description:"enable delete original file."`
	Passwd        string        `long:"pwd" description:"enter the password of the compressed file."`
	Output        path          `short:"o" long:"out" description:"set output file path (default is add suffix '_resize' to file name)."`
	Ratio         pctInt        `short:"r" long:"ratio" default:"70" description:"set resize ratio."`
	Quality       pctInt        `short:"q" long:"quality" default:"90" description:"set encoder quality."`
	ResizeMode    interpolation `long:"resize-mode" default:"lanczos3" description:"set resize interpolation mode.\nSupported:\n nearest-neighbor, bilinear, bicubic,\n mitchell-netravali, lanczos2, lanczos3."`
	Jpeg          imageJpeg     `group:"Jpeg Options"`
	Developer     developer     `group:"Developer Options"`
}

type imageJpeg struct {
	OptimizeCoding  bool      `long:"optimizer" default-mask:"true" description:"perform optimization of entropy encoding parameters."`
	ProgressiveMode bool      `long:"progressive" default-mask:"true" description:"create progressive JPEG file."`
	DCTMethod       DCTMethod `long:"dct" default:"ifast" choice:"float" choice:"ifast" choice:"islow" description:"set JPEG encoder DCT/IDCT method.\n FLOAT is floating-point: accurate, fast on fast HW.\n IFAST is faster, less accurate integer method.\n ISLOW is slow but accurate integer algorithm."`
}

type developer struct {
	ShowTime bool `long:"show-time" default-mask:"false" description:"enable show execution time."`
	Debug    bool `long:"debug" default-mask:"false" description:"enable debug mode."`
}

func InitFlags() Config {
	opts := options{
		Output:  path{CheckExists: true},
		Ratio:   pctInt{Default: 70},
		Quality: pctInt{Default: 90},
	}
	parser := flags.NewParser(&opts, flags.Default)
	args, err := parser.Parse()
	if err != nil {
		os.Exit(flags.HelpFlag)
	}
	parser.Usage = "[OPTIONS] (compressed file / directory)\nVersion: " + version

	// Parser input file
	if len(args) != 1 {
		parser.WriteHelp(os.Stderr)
		os.Exit(flags.HelpFlag)
	}
	var input string
	input, err = filepath.Abs(args[0])
	if err != nil {
		log.Error("%s", args[0])
		os.Exit(flags.IgnoreUnknown)
	}
	if !files.Exists(input) {
		log.Error("%s does not exist", input)
		os.Exit(flags.IgnoreUnknown)
	}

	// Parser OutPut file path
	output := opts.Output.Value
	if opts.Output.Value == "" {
		orgBase := filepath.Base(input)
		orgExt := filepath.Ext(orgBase)
		output = filepath.Join(
			filepath.Dir(input),
			strings.TrimSuffix(orgBase, orgExt)+"_resize"+orgExt,
		)
		if files.Exists(output) {
			log.Error("%s is exists", output)
			os.Exit(flags.IgnoreUnknown)
		}
	}

	return Config{
		AutoEncodings: opts.AutoEncodings,
		Input:         input,
		Passwd:        opts.Passwd,
		DeleteInput:   opts.DeleteInput,
		Output:        output,
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
