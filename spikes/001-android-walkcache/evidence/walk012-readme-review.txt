WALK-012 README quick-start runtime review
Scope: author-executed README sequence; independent isolated review unavailable (AI service error).
Date: 2026-11-22T16:53:41Z

$ bun --version
1.4.4
$ maestro --version
2.12.0
$ java -version
openjdk version "21.0.13.0.3" 2026-10-24
$ adb devices
List of devices attached
emulator-5554	device


$ ./gradlew clean :app:assembleDebug
> Task :app:clean
> Task :app:preBuild UP-TO-DATE
> Task :app:preDebugBuild UP-TO-DATE
> Task :app:mergeDebugNativeDebugMetadata NO-SOURCE
> Task :app:javaPreCompileDebug
> Task :app:checkDebugAarMetadata
> Task :app:generateDebugResValues
> Task :app:mapDebugSourceSetPaths
> Task :app:generateDebugResources
> Task :app:mergeDebugResources
> Task :app:packageDebugResources
> Task :app:createDebugCompatibleScreenManifests
> Task :app:extractDeepLinksDebug
> Task :app:parseDebugLocalResources
> Task :app:processDebugMainManifest
> Task :app:processDebugManifest
> Task :app:mergeDebugShaders
> Task :app:compileDebugShaders NO-SOURCE
> Task :app:generateDebugAssets UP-TO-DATE
> Task :app:mergeDebugAssets
> Task :app:compressDebugAssets
> Task :app:desugarDebugFileDependencies
> Task :app:processDebugJavaRes NO-SOURCE
> Task :app:checkDebugDuplicateClasses
> Task :app:processDebugManifestForPackage
> Task :app:mergeDebugJavaResource
> Task :app:mergeExtDexDebug
> Task :app:mergeDebugJniLibFolders
> Task :app:mergeDebugNativeLibs NO-SOURCE
> Task :app:stripDebugDebugSymbols NO-SOURCE
> Task :app:mergeLibDexDebug
> Task :app:validateSigningDebug
> Task :app:writeDebugAppMetadata
> Task :app:writeDebugSigningConfigVersions
> Task :app:processDebugResources
> Task :app:compileDebugJavaWithJavac
> Task :app:dexBuilderDebug
> Task :app:mergeDebugGlobalSynthetics
> Task :app:mergeProjectDexDebug
> Task :app:packageDebug
> Task :app:createDebugApkListingFileRedirect
> Task :app:assembleDebug

BUILD SUCCESSFUL in 785ms
33 actionable tasks: 33 executed

$ adb -s emulator-5554 install -r -g app/build/outputs/apk/debug/app-debug.apk
Performing Streamed Install
Success

$ sh scripts/fixture-mode.sh stable
Checkout fixture mode on emulator-5554: stable

$ quarantine existing generated .walkcache

$ bun ../../apps/cli/walkcache.ts remember checkout --flow evidence/repaired-checkout.yaml --config walkcache.config.json
Command: remember
Goal: checkout
Outcome: remembered
Passed: yes
Cache status: not-checked
Model calls: 0 (remember performs independent verification only)
Duration: 28042 ms
Assertions: 12
Repair count: 0
YAML: .walkcache/flows/checkout/android-e09660f45502b720f79f95a65c78ad5f48a4e11ae1ff5f52c6cb99972b12b478.yaml
YAML SHA-256: e09660f45502b720f79f95a65c78ad5f48a4e11ae1ff5f52c6cb99972b12b478
Report: .walkcache/reports/remember-1795366451528-80358.json
Latest report: .walkcache/reports/latest.json
Artifacts:
- [yaml] evidence/repaired-checkout.yaml
- [output] .walkcache/attempts/remember-1795366423486-lk4yzpvj/output.log
- [output] .walkcache/attempts/remember-1795366423486-lk4yzpvj/stdout.log
- [output] .walkcache/attempts/remember-1795366423486-lk4yzpvj/stderr.log
- [yaml] .walkcache/flows/checkout/android-e09660f45502b720f79f95a65c78ad5f48a4e11ae1ff5f52c6cb99972b12b478.yaml
- [memory] .walkcache/memory/walks.json
Source diffs: none

$ bun ../../apps/cli/walkcache.ts run checkout --config walkcache.config.json
Command: run
Goal: checkout
Outcome: passed
Passed: yes
Cache status: hit
Model calls: 0 (exact local hit: explorer and model factories were not invoked)
Duration: 29023 ms
Assertions: 12
Repair count: 0
YAML: .walkcache/flows/checkout/android-e09660f45502b720f79f95a65c78ad5f48a4e11ae1ff5f52c6cb99972b12b478.yaml
YAML SHA-256: e09660f45502b720f79f95a65c78ad5f48a4e11ae1ff5f52c6cb99972b12b478
Report: .walkcache/reports/run-1795366480581-80504.json
Latest report: .walkcache/reports/latest.json
Artifacts:
- [yaml] .walkcache/flows/checkout/android-e09660f45502b720f79f95a65c78ad5f48a4e11ae1ff5f52c6cb99972b12b478.yaml
- [output] .walkcache/reports/run-1795366451560-9lwtj7gk/output.log
- [output] .walkcache/reports/run-1795366451560-9lwtj7gk/stdout.log
- [output] .walkcache/reports/run-1795366451560-9lwtj7gk/stderr.log
Source diffs: none

$ bun ../../apps/cli/walkcache.ts inspect .walkcache/reports/latest.json --config walkcache.config.json
Command: run
Goal: checkout
Outcome: passed
Passed: yes
Cache status: hit
Model calls: 0 (exact local hit: explorer and model factories were not invoked)
Duration: 29023 ms
Assertions: 12
Repair count: 0
YAML: .walkcache/flows/checkout/android-e09660f45502b720f79f95a65c78ad5f48a4e11ae1ff5f52c6cb99972b12b478.yaml
YAML SHA-256: e09660f45502b720f79f95a65c78ad5f48a4e11ae1ff5f52c6cb99972b12b478
Artifacts:
- [yaml] .walkcache/flows/checkout/android-e09660f45502b720f79f95a65c78ad5f48a4e11ae1ff5f52c6cb99972b12b478.yaml
- [output] .walkcache/reports/run-1795366451560-9lwtj7gk/output.log
- [output] .walkcache/reports/run-1795366451560-9lwtj7gk/stdout.log
- [output] .walkcache/reports/run-1795366451560-9lwtj7gk/stderr.log
Source diffs: none

Review result: README commands reached PASS / cache HIT / 0 model calls. Independent clean-context review remains unverified because the isolated reviewer failed before execution.
