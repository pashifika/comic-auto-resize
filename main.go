// Package last_re
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
package main

import (
	"context"
	"errors"
	"os"
	"runtime"
	"time"

	"github.com/pashifika/util/errgroup"

	"github.com/pashifika/comic-auto-resize/utils/archiver"
	"github.com/pashifika/comic-auto-resize/utils/config"
	"github.com/pashifika/comic-auto-resize/utils/images"
	"github.com/pashifika/comic-auto-resize/utils/log"
	"github.com/pashifika/comic-auto-resize/utils/stream"
)

func main() {
	conf := config.InitFlags()
	log.SetDebug(conf.Developer.Debug)
	log.Debug("config: %v", conf)

	image := images.NewImageProcess()
	image.Init(conf)
	fs := archiver.NewFileSystem(conf)
	// Calculate the number of threads the program needs to use
	cpus := runtime.NumCPU()
	if cpus >= 5 {
		cpus = cpus - 1
	} else {
		cpus = 4
	}
	cache := &stream.Stream{
		File:  fs,
		Image: image,
	}
	cache.Init(cpus)
	fs.Extensions = image.Extensions()
	fs.Cache = cache

	now := time.Now()
	eg, ctx := errgroup.WithContext(context.Background(), cpus)
	err := fs.Open(ctx, conf.Input, eg)
	if err != nil {
		log.Fatal("%s", err)
	}

	err = cache.Convert(ctx, eg)
	if err != nil && !errors.Is(err, context.Canceled) {
		log.Fatal("%s", err)
	}

	err = fs.SaveToDisk()
	if err != nil {
		log.Fatal("%s", err)
	}
	if conf.DeleteInput {
		err = os.Remove(conf.Input)
		if err != nil {
			log.Error("%s", err)
		}
	}
	if conf.Developer.ShowTime || conf.Developer.Debug {
		execTime := float64(time.Since(now).Milliseconds()) / 1000
		log.Info("execution time: %.2f/s", execTime)
	}
}
