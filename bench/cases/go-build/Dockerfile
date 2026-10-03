FROM localhost:5000/library/golang:1.25
ENV CGO_ENABLED=0
WORKDIR /src
COPY . .
RUN go build -o /out/app .
