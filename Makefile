.PHONY: all build test cross-armv7 cross-aarch64 ipad-test deploy-plc bench-plc hil-plc

CARGO := PATH="$(HOME)/.cargo/bin:$(HOME)/.local/bin:$(PATH)" cargo
PLC_SSH := luckfox-usb
PLC_DIR := /userdata/plc-auto

all: build test

build:
	npm run build
	$(CARGO) build --workspace
	$(CARGO) build --release -p logic-worker -p plc-bench --features wasmtime-engine

test:
	$(CARGO) test --workspace
	$(CARGO) test -p logic-worker --features wasmtime-engine
	npm test
	cd apps/ipad && swift test

ipad-test:
	cd apps/ipad && swift test

cross-armv7:
	$(CARGO) zigbuild --release --target armv7-unknown-linux-gnueabihf.2.36 --workspace

cross-aarch64:
	$(CARGO) zigbuild --release --target aarch64-unknown-linux-gnu.2.36 --workspace

deploy-plc: cross-armv7
	npm run build --workspace=@plc-auto/console
	ssh $(PLC_SSH) "mkdir -p $(PLC_DIR)/bin $(PLC_DIR)/profiles $(PLC_DIR)/packages $(PLC_DIR)/data $(PLC_DIR)/benchmarks $(PLC_DIR)/console"
	scp target/armv7-unknown-linux-gnueabihf/release/plc-supervisor target/armv7-unknown-linux-gnueabihf/release/plc-worker target/armv7-unknown-linux-gnueabihf/release/plc-bench $(PLC_SSH):$(PLC_DIR)/bin/
	scp profiles/demo-lyra/site-profile.json profiles/demo-lyra/bench-w3-profile.json profiles/demo-lyra/keys/test-bench.pub.hex profiles/demo-lyra/keys/test-bench.seed.hex $(PLC_SSH):$(PLC_DIR)/profiles/
	scp profiles/demo-lyra/packages/*.armv7-linux-gnueabihf.json profiles/demo-lyra/packages/*.quickjs.json $(PLC_SSH):$(PLC_DIR)/packages/
	scp packages/console/dist-web/* $(PLC_SSH):$(PLC_DIR)/console/

hil-plc:
	ssh $(PLC_SSH) "$(PLC_DIR)/bin/plc-bench hil --profile $(PLC_DIR)/profiles/site-profile.json --pubkey $(PLC_DIR)/profiles/test-bench.pub.hex --pkg-dir $(PLC_DIR)/packages --worker-bin $(PLC_DIR)/bin/plc-worker --supervisor-bin $(PLC_DIR)/bin/plc-supervisor --data-dir $(PLC_DIR)/data --out $(PLC_DIR)/benchmarks/hil"
