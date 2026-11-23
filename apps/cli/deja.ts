#!/usr/bin/env bun
import { runCli } from './src/main.ts'

process.exitCode = await runCli(Bun.argv.slice(2))
