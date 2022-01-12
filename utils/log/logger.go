// Package log
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
package log

import (
	"fmt"
	"os"
)

type Logger struct {
	stdOut, stdErr *os.File
	debug          bool
}

var stdLog *Logger

func init() {
	stdLog = &Logger{
		stdOut: os.Stdout,
		stdErr: os.Stderr,
		debug:  _debug,
	}
}

func SetDebug(mode bool) {
	stdLog.debug = mode
}

func Debug(format string, a ...interface{}) {
	if stdLog.debug {
		_, _ = stdLog.stdOut.WriteString("[DEBUG] " + fmt.Sprintf(format, a...) + "\n")
	}
}

func Info(format string, a ...interface{}) {
	_, _ = stdLog.stdOut.WriteString("[INFO] " + fmt.Sprintf(format, a...) + "\n")
}

func Warn(format string, a ...interface{}) {
	_, _ = stdLog.stdErr.WriteString("[WARN] " + fmt.Sprintf(format, a...) + "\n")
}

func Error(format string, a ...interface{}) {
	_, _ = stdLog.stdErr.WriteString("[ERROR] " + fmt.Sprintf(format, a...) + "\n")
}

func Fatal(format string, a ...interface{}) {
	_, _ = stdLog.stdErr.WriteString("[FATAL] " + fmt.Sprintf(format, a...) + "\n")
}

func Panic(format string, a ...interface{}) {
	_, _ = stdLog.stdErr.WriteString("[PANIC] " + fmt.Sprintf(format, a...) + "\n")
}
