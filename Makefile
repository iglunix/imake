RUSTC=rustc

target/debug/imake: src/main.rs
	$(RUSTC) -o $@ $^

clean:
	rm -f target/debug/imake
