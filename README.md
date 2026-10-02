# Sandcastle

On the beach the are crabs and sandcastles.

Sandcastle its an experiment to build docker/podman image build in pure Rust 🦀

# Run conformance tests (very slow):
## Source: https://github.com/openshift/imagebuilder
```
docker rmi mirror.gcr.io/alpine; docker pull mirror.gcr.io/alpine
docker rmi mirror.gcr.io/busybox; docker pull mirror.gcr.io/busybox
docker rmi public.ecr.aws/docker/library/centos:7; docker pull public.ecr.aws/docker/library/centos:7
docker rmi mirror.gcr.io/debian; docker pull mirror.gcr.io/debian
docker rmi registry.fedoraproject.org/fedora-minimal; docker pull registry.fedoraproject.org/fedora-minimal
docker rmi registry.fedoraproject.org/fedora-minimal:44-x86_64; docker pull registry.fedoraproject.org/fedora-minimal:44-x86_64
docker rmi registry.fedoraproject.org/fedora-minimal:44-aarch64; docker pull registry.fedoraproject.org/fedora-minimal:44-aarch64
docker rmi mirror.gcr.io/golang:1.25; docker pull mirror.gcr.io/golang:1.25
docker rmi mirror.gcr.io/nginx; docker pull mirror.gcr.io/nginx
chmod -R go-w ./dockerclient/testdata # TODO: Rewrote to Rust
env DOCKER_API_VERSION=1.44 go test ./dockerclient -tags conformance -timeout 30m  # TODO: Rewrote to Rust
```