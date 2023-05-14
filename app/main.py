import asyncio
import os
import os.path
import shutil
import time
from typing import AsyncIterator, Optional
import uuid

from fastapi import APIRouter, FastAPI, File, Form, HTTPException, UploadFile, status
from fastapi.responses import StreamingResponse

import aiofiles
import aiofiles.os
import aiofiles.ospath
from fastapi_utils.tasks import repeat_every
import sentry_sdk

from config import env_config


if env_config.SENTRY_DSN:
    sentry_sdk.init(
        env_config.SENTRY_DSN,
    )


router = APIRouter(tags=["converter"])


@router.post("/")
async def convert(
    file: Optional[UploadFile] = File(None),
    format: Optional[str] = Form(None),
):
    if file is None or format is None:
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST, detail="File and format required!"
        )

    format_lower = format.lower()
    if format_lower not in ["epub", "mobi"]:
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST, detail="Bad format!"
        )

    temp_uuid = uuid.uuid1()

    temp_filename = str(temp_uuid) + ".fb2"
    converted_temp_filename = str(temp_uuid) + "." + format_lower

    try:
        async with aiofiles.open(temp_filename, "wb") as f:
            while content := await file.read(1024):
                if isinstance(content, str):
                    content = content.encode()

                await f.write(content)

        proc = await asyncio.create_subprocess_exec(
            "./bin/fb2c",
            "convert",
            "--to",
            format,
            temp_filename,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )

        _, stderr = await proc.communicate()
    finally:
        await aiofiles.os.remove(temp_filename)

    if proc.returncode != 0 or len(stderr) != 0:
        try:
            await aiofiles.os.remove(converted_temp_filename)
        except FileNotFoundError:
            pass

        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST, detail="Can't convert!"
        )

    async def result_iterator() -> AsyncIterator[bytes]:
        try:
            async with aiofiles.open(converted_temp_filename, "rb") as f:
                while data := await f.read(2048):
                    yield data
        finally:
            await aiofiles.os.remove(converted_temp_filename)

    return StreamingResponse(result_iterator())


@router.get("/healthcheck")
async def healthcheck():
    return "Ok!"


app = FastAPI()

app.include_router(router)


@app.on_event("startup")
@repeat_every(seconds=5 * 60, raise_exceptions=True)
def remove_temp_files():
    current_time = time.time()

    try:
        os.remove("./conversion.log")
    except IOError:
        pass

    for f in os.listdir("/tmp/"):
        target_path = f"/tmp/{f}"

        is_file = os.path.isfile(target_path)

        try:
            creation_time = os.path.getctime(target_path)
        except FileNotFoundError:
            continue

        if (current_time - creation_time) // 3600 >= 3:
            if is_file:
                os.remove(target_path)
            else:
                shutil.rmtree(target_path)
