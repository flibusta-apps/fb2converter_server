FROM python:3.11-slim AS build-image

RUN apt-get update \
    && apt-get install --no-install-recommends -y unzip gcc build-essential python3-dev \
    && rm -rf /var/lib/apt/lists/*

# Get converter bin
WORKDIR  /root/fb2converter
ADD https://github.com/rupor-github/fb2converter/releases/download/v1.70.0/fb2c_linux_amd64.zip ./
RUN unzip fb2c_linux_amd64.zip

# Install requirements
WORKDIR /root/poetry
COPY pyproject.toml poetry.lock /root/poetry/

RUN pip install poetry wheel --no-cache-dir \
    && poetry export --without-hashes > requirements.txt

ENV VENV_PATH=/opt/venv
RUN python -m venv $VENV_PATH \
    && . /opt/venv/bin/activate \
    && pip install -r requirements.txt --no-cache-dir


FROM python:3.11-slim AS runtime-image

WORKDIR /app

COPY ./app/ ./

ENV VENV_PATH=/opt/venv
ENV PATH="$VENV_PATH/bin:$PATH"

COPY --from=build-image /root/fb2converter/fb2c /app/bin/
COPY --from=build-image /root/fb2converter/kindlegen /app/bin/

COPY --from=build-image $VENV_PATH $VENV_PATH
COPY ./scripts/healthcheck.py /root/healthcheck.py

EXPOSE 8080

CMD uvicorn main:app --host 0.0.0.0 --port 8080 --loop uvloop --timeout-keep-alive 600
