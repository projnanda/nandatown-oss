import os
from collections.abc import AsyncIterable
from typing import Any

import httpx
from google.adk.agents import Agent
from google.adk.errors.already_exists_error import AlreadyExistsError
from google.adk.runners import Runner
from google.genai import types as genai_types
from nasiko_adk_sessions import session_service_from_env


def search_wikipedia(query: str) -> str:
    """Search Wikipedia for a summary of a topic."""
    resp = httpx.get(
        f"https://en.wikipedia.org/api/rest_v1/page/summary/{query.replace(' ', '_')}",
        follow_redirects=True,
        timeout=10.0,
    )
    if resp.status_code != 200:
        return f"No Wikipedia article found for '{query}'."
    return resp.json().get("extract", "No extract available.")


APP_NAME = "nasiko_research"


class ResearchAgent:
    SUPPORTED_CONTENT_TYPES = ["text", "text/plain"]

    def __init__(self):
        self._agent = Agent(
            model="gemini-2.0-flash",
            name="research_agent",
            instruction=(
                "You are a research assistant. Use the search_wikipedia tool to "
                "answer questions accurately and concisely."
            ),
            tools=[search_wikipedia],
        )
        self._session_service = session_service_from_env()
        self._runner = Runner(
            agent=self._agent,
            app_name=APP_NAME,
            session_service=self._session_service,
        )

    async def _ensure_session(self, context_id: str) -> None:
        # A durable session outlives the process and is shared by replicas, so a later turn (or
        # another replica) finds it already there; creating it again is refused.
        existing = await self._session_service.get_session(
            app_name=APP_NAME, user_id="user", session_id=context_id
        )
        if existing is not None:
            return
        try:
            await self._session_service.create_session(
                app_name=APP_NAME, user_id="user", session_id=context_id
            )
        except AlreadyExistsError:
            # Another replica created it between the get and the create; that session is ours too.
            pass

    async def stream(self, query: str, context_id: str) -> AsyncIterable[dict[str, Any]]:
        yield {
            "is_task_complete": False,
            "require_user_input": False,
            "content": "Researching...",
        }
        await self._ensure_session(context_id)
        final_text = ""
        async for event in self._runner.run_async(
            user_id="user",
            session_id=context_id,
            new_message=genai_types.Content(
                role="user",
                parts=[genai_types.Part(text=query)],
            ),
        ):
            if event.is_final_response() and event.content and event.content.parts:
                final_text = event.content.parts[0].text
        yield {
            "is_task_complete": True,
            "require_user_input": False,
            "content": final_text or "No answer found.",
        }
