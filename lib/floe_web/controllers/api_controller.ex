defmodule FloeWeb.ApiController do
  use FloeWeb, :controller

  def whip(conn, params) do
    IO.inspect(params)

    render(conn, :whip)
  end
end
